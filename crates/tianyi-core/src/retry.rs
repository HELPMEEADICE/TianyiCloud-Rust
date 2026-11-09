//! 请求级重试与指数退避
//!
//! 只给**幂等**请求用（无签名 GET / 带签名 GET）。POST 表单与上传分片各自有
//! 独立的失败处理（分片重试、续传），不走这里，避免把非幂等写操作重放。
//!
//! 退避序列：200 ms / 600 ms / 1800 ms（`base * 3^(n-1)`），每次带 ±20% 抖动。
//! 抖动用于打散并发任务的重试时刻，避免同一时刻集中重放。

use crate::error::{Error, Result};
use std::future::Future;
use std::time::Duration;

/// 默认总尝试次数（含首次请求）
pub(crate) const DEFAULT_ATTEMPTS: usize = 3;
/// 默认首次退避基准
pub(crate) const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(200);
/// 默认抖动比例（±20%）
pub(crate) const DEFAULT_JITTER: f64 = 0.2;

/// 天翼云「系统繁忙/稍后重试」类字符串错误码
///
/// 说明：`crate::api` 与 `crate::client::consts` 里只有 `UserInvalidOpenToken`
/// 一个语义明确的码值，没有任何可确证的「系统繁忙」码值，因此这里保持为空，
/// 不臆造码值。后续在服务端响应里确认码值后，往这里追加即可生效。
const BUSY_CODE_NAMES: &[&str] = &[];

/// 重试策略
///
/// 默认值即生产配置：[`DEFAULT_ATTEMPTS`] 次尝试、[`DEFAULT_BASE_DELAY`] 首次退避、
/// ±[`DEFAULT_JITTER`] 抖动。测试可注入更小的 `base_delay`/`jitter` 以缩短耗时。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// 总尝试次数（含首次）
    pub attempts: usize,
    /// 首次退避基准
    pub base_delay: Duration,
    /// 抖动比例（0.2 表示 ±20%）
    pub jitter: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: DEFAULT_ATTEMPTS,
            base_delay: DEFAULT_BASE_DELAY,
            jitter: DEFAULT_JITTER,
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt` 次失败（从 1 开始）之后应等待的时长
    ///
    /// `base * 3^(attempt-1)`，再叠加 ±jitter 的随机抖动。
    pub(crate) fn delay_for(&self, attempt: usize) -> Duration {
        let exp = attempt.saturating_sub(1).min(8) as u32;
        let base = self.base_delay.mul_f64(3f64.powi(exp as i32));
        let factor = 1.0 + (rand::random::<f64>() * 2.0 - 1.0) * self.jitter.clamp(0.0, 1.0);
        base.mul_f64(factor.max(0.01))
    }
}

/// 判断错误是否值得重试
///
/// 只重试三类：网络层瞬时错误（超时/连接/请求未发出）、HTTP 5xx/429、
/// 以及可确证的「系统繁忙」业务码。以下一律不重试：
/// - 4xx 业务错误（重试也不会变好）
/// - 会话失效（走既有 token/session 刷新链路，见 `Error::is_session_invalid`）
/// - 用户暂停/取消（`Error::Paused` / `Error::Canceled`，重试会违背用户意图）
pub(crate) fn is_retryable(err: &Error) -> bool {
    if err.is_interrupted() {
        return false;
    }
    if err.is_session_invalid() {
        return false;
    }
    match err {
        // 请求构造阶段的错误（is_builder）重试无意义
        Error::Http(e) => !e.is_builder() && (e.is_timeout() || e.is_connect() || e.is_request()),
        Error::HttpStatus { status, .. } => *status == 429 || (500..600).contains(status),
        Error::ApiCoded { code_name, .. } => BUSY_CODE_NAMES
            .iter()
            .any(|k| code_name.eq_ignore_ascii_case(k)),
        _ => false,
    }
}

/// 按策略重试一个异步操作的工厂
///
/// `op` 每次尝试都会被重新调用，因此请求必须自带签名生成（或只读），
/// 这样每次重试都能拿到新的 `Date` / `Signature`。
pub(crate) async fn with_retry<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= policy.attempts || !is_retryable(&e) {
                    if attempt > 1 {
                        log::warn!("request failed after {attempt} attempt(s): {e}");
                    }
                    return Err(e);
                }
                let delay = policy.delay_for(attempt);
                log::warn!(
                    "request failed (attempt {}/{}), retry in {:?}: {e}",
                    attempt,
                    policy.attempts,
                    delay
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            attempts: DEFAULT_ATTEMPTS,
            base_delay: Duration::from_millis(1),
            jitter: 0.0,
        }
    }

    /// 默认值必须是生产配置：3 次尝试、200ms 起、±20% 抖动
    #[test]
    fn test_default_policy_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.attempts, 3);
        assert_eq!(p.base_delay, Duration::from_millis(200));
        assert!((p.jitter - 0.2).abs() < f64::EPSILON);
    }

    /// 退避序列为 200/600/1800 ms，抖动在 ±20% 内
    #[test]
    fn test_backoff_sequence_and_jitter_bounds() {
        let p = RetryPolicy::default();
        for (attempt, expect_ms) in [(1usize, 200u64), (2, 600), (3, 1800)] {
            let expect = Duration::from_millis(expect_ms);
            let low = expect.mul_f64(0.8);
            let high = expect.mul_f64(1.2);
            for _ in 0..32 {
                let d = p.delay_for(attempt);
                assert!(
                    d >= low && d <= high,
                    "第 {attempt} 次退避 {d:?} 超出 {low:?}..={high:?}"
                );
            }
        }
    }

    /// 只有瞬时网络/5xx/429 才重试；4xx、会话失效、暂停取消一律不重试
    #[test]
    fn test_is_retryable_classification() {
        assert!(is_retryable(&Error::HttpStatus {
            status: 500,
            body: String::new()
        }));
        assert!(is_retryable(&Error::HttpStatus {
            status: 503,
            body: String::new()
        }));
        assert!(is_retryable(&Error::HttpStatus {
            status: 429,
            body: String::new()
        }));
        assert!(!is_retryable(&Error::HttpStatus {
            status: 400,
            body: String::new()
        }));
        assert!(!is_retryable(&Error::HttpStatus {
            status: 403,
            body: String::new()
        }));

        assert!(!is_retryable(&Error::Paused));
        assert!(!is_retryable(&Error::Canceled));
        assert!(!is_retryable(&Error::Session("session invalid".into())));
        assert!(!is_retryable(&Error::api_coded(
            0,
            "InvalidSessionKey",
            "session invalid"
        )));
        assert!(!is_retryable(&Error::api_coded(0, "FileNotFound", "nope")));
        assert!(!is_retryable(&Error::Api("plain".into())));
    }

    /// 两次失败后成功：总尝试 3 次并返回 Ok
    #[tokio::test]
    async fn test_with_retry_succeeds_on_third_attempt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<&str> = with_retry(&fast_policy(), || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(Error::HttpStatus {
                        status: 500,
                        body: "boom".to_string(),
                    })
                } else {
                    Ok("ok")
                }
            }
        })
        .await;

        assert_eq!(out.expect("第三次应成功"), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "总尝试次数必须是 3");
    }

    /// 不可重试错误只尝试一次
    #[tokio::test]
    async fn test_with_retry_stops_on_business_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<()> = with_retry(&fast_policy(), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(Error::HttpStatus {
                    status: 400,
                    body: "bad request".to_string(),
                })
            }
        })
        .await;

        assert!(matches!(out, Err(Error::HttpStatus { status: 400, .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx 只应尝试一次");
    }

    /// 用户取消原样返回，且不重试
    #[tokio::test]
    async fn test_with_retry_returns_canceled_as_is() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let out: Result<()> = with_retry(&fast_policy(), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(Error::Canceled)
            }
        })
        .await;

        assert!(matches!(out, Err(Error::Canceled)), "应原样返回 Canceled");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "取消不应重试");
    }
}
