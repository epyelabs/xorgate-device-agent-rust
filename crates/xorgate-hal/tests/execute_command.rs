//! `ts:test/commands.test.ts` `describe("executeCommand")` (4 `it()`, `:156-222`)
//! through [`RecordingRunner`] — no sudo is ever run. The argv expectations are
//! the TS ones except the O3 updater unit (`xorgate-updater-rs.service`).

use xorgate_core::commands::{DeviceCommandName, RunResult};
use xorgate_hal::privileged::{execute_command, RecordingRunner};

#[tokio::test]
async fn maps_each_command_to_a_non_interactive_systemctl_call() {
    let r = RecordingRunner::new();
    execute_command(DeviceCommandName::Reboot, &r).await;
    execute_command(DeviceCommandName::PowerOff, &r).await;
    assert_eq!(
        r.calls(),
        vec![
            vec!["sudo", "-n", "systemctl", "reboot"],
            vec!["sudo", "-n", "systemctl", "poweroff"],
        ]
    );
}

#[tokio::test]
async fn starts_the_updater_unit_with_the_exact_argv_the_sudoers_grant_allows() {
    // TS: ["sudo", ["-n","systemctl","start","--no-block","xorgate-updater.service"]];
    // the Rust agent targets its own unit (O3), same words otherwise.
    let r = RecordingRunner::new();
    execute_command(DeviceCommandName::Update, &r).await;
    assert_eq!(
        r.calls(),
        vec![vec![
            "sudo",
            "-n",
            "systemctl",
            "start",
            "--no-block",
            "xorgate-updater-rs.service"
        ]]
    );
}

#[tokio::test]
async fn never_routes_the_in_process_read_only_commands_to_sudo() {
    let r = RecordingRunner::new();
    for c in [DeviceCommandName::Refresh, DeviceCommandName::ReadSms] {
        let res = execute_command(c, &r).await;
        assert!(res.failed);
    }
    assert!(r.calls().is_empty());
}

#[tokio::test]
async fn surfaces_a_refused_sudo_rather_than_throwing() {
    let r = RecordingRunner::new();
    r.answer(RunResult {
        stdout: String::new(),
        stderr: "sudo: a password is required".into(),
        exit_code: Some(1),
        failed: true,
        not_found: false,
    });
    let res = execute_command(DeviceCommandName::Reboot, &r).await;
    assert!(res.failed);
    assert!(res.stderr.contains("password is required"));
}
