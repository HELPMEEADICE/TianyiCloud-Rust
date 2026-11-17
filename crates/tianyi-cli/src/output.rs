//! 输出协议：人类可读 / JSON 单次 / NDJSON 事件流
//!
//! 三条铁律（脚本与 Agent 直接依赖）：
//! 1. **stdout 只放结果**：JSON 模式下一行到多行 JSON，人类模式下只有表格与文本，
//!    绝没有进度条、彩色转义或日志；
//! 2. **stderr 放诊断**：日志、进度、二维码、提示一律走 stderr，且 JSON 模式下不带
//!    ANSI 颜色（重定向到文件时更干净）；
//! 3. **结构稳定**：成功是 {success:true,data:...,error:null}，失败是
//!    {success:false,data:null,error:{code,message,retryable}}，字段名与含义都不随版本变。

use crate::error::{CliError, CliResult};
use serde_json::{json, Value};
use std::io::Write;

/// 输出模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutMode {
    /// 人类可读（默认）
    Human,
    /// 单次命令的 JSON 信封
    Json,
    /// 一行一条 JSON 事件（带关联 id），供持续会话使用
    Ndjson,
}

impl OutMode {
    /// 是否机器可读
    pub fn is_machine(self) -> bool {
        !matches!(self, OutMode::Human)
    }
}

/// 一条命令的结果：既能给人看，也能序列化成 JSON
///
/// 两个字段一起返回而不是「命令自己打印」：这样同一条命令的人读文案与机器可读数据永远
/// 来自同一次计算，不会出现 JSON 里 12 个文件、表格里 11 行的错位。
#[derive(Debug, Clone)]
pub struct Output {
    /// 写进 JSON 的 data 字段
    pub data: Value,
    /// 人类可读文本（已含换行）
    pub human: String,
}

impl Output {
    pub fn new(data: Value, human: impl Into<String>) -> Self {
        Output {
            data,
            human: human.into(),
        }
    }

    /// 只有人读文案（例如 logout 成功的提示），JSON 里给 null
    pub fn text(human: impl Into<String>) -> Self {
        Output {
            data: Value::Null,
            human: human.into(),
        }
    }
}

/// 成功信封
pub fn success_value(data: Value) -> Value {
    json!({ "success": true, "data": data, "error": Value::Null })
}

/// 失败信封
pub fn error_value(err: &CliError) -> Value {
    let mut payload = json!({
        "code": err.code.as_str(),
        "message": err.message,
        "retryable": err.retryable(),
    });
    if let (Some(details), Some(map)) = (&err.details, payload.as_object_mut()) {
        map.insert("details".to_string(), details.clone());
    }
    json!({ "success": false, "data": Value::Null, "error": payload })
}

/// 稳定地美化 JSON（键序由 serde_json 的 BTreeMap 决定，同一份数据每次输出一致）
pub fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
}

/// 单行 JSON（NDJSON 用）
pub fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// 真正的输出器
///
/// 只碰 stdout/stderr，不做缓冲策略以外的聪明事：管道被提前关闭（例如 head -1）时
/// 安静收工，不 panic、不打印 broken pipe。
#[derive(Debug, Clone)]
pub struct Printer {
    mode: OutMode,
    quiet: bool,
}

impl Printer {
    pub fn new(mode: OutMode, quiet: bool) -> Self {
        Printer { mode, quiet }
    }

    pub fn mode(&self) -> OutMode {
        self.mode
    }

    /// 是否静默（--quiet）
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    /// 输出一条成功结果
    pub fn emit(&self, out: &Output) -> CliResult<()> {
        match self.mode {
            OutMode::Human => write_stdout(&out.human),
            OutMode::Json => {
                write_stdout(&format!("{}\n", pretty(&success_value(out.data.clone()))))
            }
            // NDJSON 的单次命令等价于「一行一个信封」，关联 id 由上层 session 负责
            OutMode::Ndjson => {
                write_stdout(&format!("{}\n", compact(&success_value(out.data.clone()))))
            }
        }
    }

    /// 输出一条失败结果（stdout 是协议的一部分，所以错误信封也必须写 stdout）
    pub fn emit_error(&self, err: &CliError) {
        match self.mode {
            OutMode::Human => {
                let _ = write_stderr(&format!("错误：{}（{}）\n", err.message, err.code.as_str()));
            }
            OutMode::Json | OutMode::Ndjson => {
                let _ = write_stdout(&format!("{}\n", compact(&error_value(err))));
            }
        }
    }

    /// NDJSON：带关联 id 的成功记录
    pub fn emit_ok_with_id(&self, id: u64, data: Value) {
        let mut v = success_value(data);
        if let Some(map) = v.as_object_mut() {
            map.insert("id".to_string(), json!(id));
        }
        let _ = write_stdout(&format!("{}\n", compact(&v)));
    }

    /// NDJSON：带关联 id 的失败记录
    pub fn emit_error_with_id(&self, id: u64, err: &CliError) {
        let mut v = error_value(err);
        if let Some(map) = v.as_object_mut() {
            map.insert("id".to_string(), json!(id));
        }
        let _ = write_stdout(&format!("{}\n", compact(&v)));
    }

    /// 诊断信息（stderr）。JSON 模式也照写：它不污染 stdout，反而方便排错。
    pub fn diag(&self, line: &str) {
        if self.quiet {
            return;
        }
        let _ = write_stderr(&format!("{line}\n"));
    }

    /// 进度行（stderr，覆盖当前行）
    ///
    /// 人类模式才有回退控制符；机器模式每 200ms 刷一行会把日志刷爆，因此进度只在
    /// 人类模式与显式开启时输出，机器模式的进度改由 NDJSON 事件表达。
    pub fn progress(&self, line: &str) {
        // NDJSON 模式下 stdout 是事件流，进度绝不能出现（会打破「一行一条响应」）；
        // JSON 模式允许写 stderr——协议只约束 stdout。
        if self.quiet || matches!(self.mode, OutMode::Ndjson) {
            return;
        }
        let _ = write_stderr(&format!("\r{line}\x1b[K"));
    }

    /// 结束一行进度（换行，避免后续输出接在进度行后面）
    pub fn progress_done(&self) {
        if self.quiet || matches!(self.mode, OutMode::Ndjson) {
            return;
        }
        let _ = write_stderr("\n");
    }
}

fn write_stdout(text: &str) -> CliResult<()> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|_| stdout.flush())
    {
        Ok(()) => Ok(()),
        // 管道被下游提前关闭不是错误：安静退出即可
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(CliError::from(e)),
    }
}

fn write_stderr(text: &str) -> std::io::Result<()> {
    let mut stderr = std::io::stderr().lock();
    match stderr
        .write_all(text.as_bytes())
        .and_then(|_| stderr.flush())
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn parse(v: &Value) -> Value {
        serde_json::from_str(&compact(v)).expect("必须是合法 JSON")
    }

    #[test]
    fn success_envelope_has_exactly_the_documented_shape() {
        let v = parse(&success_value(json!({"authenticated": true})));
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["data"]["authenticated"], json!(true));
        assert!(v["error"].is_null(), "成功时 error 必须是 null，不是缺字段");
        // 顶层只允许这三个键
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["data", "error", "success"]);
    }

    #[test]
    fn error_envelope_has_code_message_retryable() {
        let err = CliError::new(ErrorCode::AuthExpired, "Authentication expired");
        let v = parse(&error_value(&err));
        assert_eq!(v["success"], json!(false));
        assert!(v["data"].is_null());
        assert_eq!(v["error"]["code"], json!("AUTH_EXPIRED"));
        assert_eq!(v["error"]["message"], json!("Authentication expired"));
        assert_eq!(v["error"]["retryable"], json!(true));
        assert!(
            v["error"].get("details").is_none(),
            "没有 details 时不该出现该字段"
        );
    }

    #[test]
    fn details_are_preserved_when_present() {
        let err = CliError::new(ErrorCode::CaptchaRequired, "need captcha")
            .with_details(json!({"captcha_path": "/tmp/captcha.png"}));
        let v = parse(&error_value(&err));
        assert_eq!(
            v["error"]["details"]["captcha_path"],
            json!("/tmp/captcha.png")
        );
    }

    #[test]
    fn output_is_jq_parsable_single_line_for_machine_modes() {
        let v = success_value(json!({"items": [1, 2, 3]}));
        let line = compact(&v);
        assert!(!line.contains('\n'), "NDJSON 一条必须在一行内");
        assert!(serde_json::from_str::<Value>(&line).is_ok());
    }

    #[test]
    fn numbers_are_integers_so_no_nan_can_leak() {
        let v = success_value(json!({
            "size": 123i64,
            "free": 456i64,
            "ratio_permille": 500u64
        }));
        let text = compact(&v);
        assert!(!text.contains("NaN"));
        // data 里不能有 null（顶层信封的 error:null 是协议的一部分，不在此列）
        assert!(!compact(&v["data"]).contains("null"));
    }
}
