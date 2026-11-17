//! Windows 环境自检（不需要云盘账号，任何 Windows 机器都能跑）
//!
//! 这里验证的是「挂载层赖以判断网络驱动器的那几个 Win32 调用真的按预期工作」：
//! 如果 GetDriveTypeW 的绑定写错了，DRIVE_REMOTE 的验收就毫无意义。

#![cfg(windows)]

use tianyi_mount::mount::{
    backend_available, default_drive_letter, drive_in_use, drive_type_of, free_drive_letters,
    parse_mount_target, MountTarget, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_REMOTE,
};

#[test]
fn get_drive_type_reports_local_disks_as_fixed() {
    // C: 在任何 Windows 上都是本地盘；这里同时还证明我们的 FFI 调用参数（宽字符 + 结尾 0）是对的
    assert_eq!(
        drive_type_of("C:\\"),
        DRIVE_FIXED,
        "C:\\ 必须是 DRIVE_FIXED"
    );
}

#[test]
fn a_plain_directory_is_not_a_network_drive() {
    let tmp = std::env::temp_dir();
    let path = tmp.to_string_lossy().to_string();
    assert_ne!(
        drive_type_of(&path),
        DRIVE_REMOTE,
        "普通目录不可能是网络驱动器：{path}"
    );
}

#[test]
fn free_drive_letters_are_really_free() {
    for letter in free_drive_letters() {
        assert!(
            letter.len() == 2 && letter.ends_with(':'),
            "格式非法：{letter}"
        );
        let l = letter.as_bytes()[0];
        assert!((b'D'..=b'Z').contains(&l), "不应建议系统保留盘符：{letter}");
        assert_eq!(
            drive_type_of(&format!("{letter}\\")),
            DRIVE_NO_ROOT_DIR,
            "{letter} 被报告为空闲，但系统认为它存在"
        );
        assert!(
            !drive_in_use(l),
            "{letter} 被报告为空闲，但 drive_in_use 说被占用"
        );
    }
}

#[test]
fn default_drive_letter_is_usable() {
    let d = default_drive_letter();
    assert!(d.len() == 2 && d.ends_with(':'), "{d}");
    let target = parse_mount_target(&d).expect("默认盘符必须能通过校验");
    assert!(matches!(target, MountTarget::Drive(_)));
}

#[test]
fn system_drive_letters_are_rejected() {
    for bad in ["C:", "B:", "A:"] {
        assert!(
            parse_mount_target(bad).is_err(),
            "{bad} 必须被拒绝（系统保留盘符）"
        );
    }
}

#[test]
fn winfsp_availability_is_reported_diagnosably() {
    // 装了 WinFsp 就要返回 Ok；没装必须返回带安装指引的中文提示，而不是 panic
    match backend_available() {
        Ok(()) => {}
        Err(msg) => {
            assert!(msg.contains("WinFsp"), "错误信息必须可诊断：{msg}");
            assert!(
                msg.contains("https://winfsp.dev"),
                "必须给出安装地址：{msg}"
            );
        }
    }
}
