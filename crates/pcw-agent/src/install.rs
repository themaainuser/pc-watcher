//! Scheduled-task install/uninstall (plan Task 13, spec §6).
//!
//! `task_xml` and `utf16_le_bom` are pure and unit-testable; `install` and
//! `uninstall` drive `schtasks` and are only ever run by an elevated,
//! explicit `pcw-agent.exe --install` / `--uninstall` invocation (never from
//! tests or the agent itself).

use std::path::{Path, PathBuf};
use std::{env, fs};

use pcw_foreground::is_elevated;

/// Task name in the system scheduler (spec §6).
const TASK_NAME: &str = "PC-Watcher";
/// Task-store command binary (PATH lookup, always present on Windows).
const SCHTASKS: &str = "schtasks.exe";

/// The exact task XML of spec §6: logon trigger for the installing user,
/// `InteractiveToken`, `HighestAvailable`, no battery stop, `PT0S` (the
/// default 72 h limit would kill the agent after 3 days), priority 5,
/// one restart every minute up to 3 times. `{user}` and `{exe}` are
/// XML-escaped (`&`, `<`, `>`, `"`).
const TASK_XML_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>PC-Watcher agent</Description></RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{exe}</Command></Exec>
  </Actions>
</Task>
"#;

/// The final task XML for `exe` started at logon of `user` (both XML-escaped).
#[must_use]
pub fn task_xml(exe: &Path, user: &str) -> String {
    TASK_XML_TEMPLATE
        .replace("{user}", &xml_escape(user))
        .replace("{exe}", &xml_escape(&exe.to_string_lossy()))
}

/// `&` first, then the angle brackets and the quote (spec §6: the owner's
/// Path may contain `&`, e.g. `C:\Users\K & Co\pcw-agent.exe`).
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// UTF-16 LE bytes with the `FF FE` BOM required by `schtasks /XML` for
/// `encoding="UTF-16"`.
#[must_use]
pub fn utf16_le_bom(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + 2 * text.len());
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

/// Installs the auto-start task (spec §6):
/// ends a running agent, copies both exes to `<data_dir>\bin\`, registers the
/// task from a UTF-16 LE XML and starts it.
pub fn install() -> Result<(), String> {
    if !is_elevated() {
        return Err("run from an elevated terminal".to_string());
    }

    // A leftover running agent holds the \bin\ exes; ending it is best-effort
    // (`schtasks /End` fails when the task does not exist yet).
    let _ = schtasks_output(&["/End", "/TN", TASK_NAME]);

    let bin_dir = install_bin_exes()?;
    let xml = task_xml(&bin_dir.join("pcw-agent.exe"), &user_name()?);
    let xml_file = write_temp_xml(&xml)?;
    let xml_arg = xml_file.to_string_lossy().to_string();

    expect_ok(
        schtasks_output(&["/Create", "/TN", TASK_NAME, "/XML", &xml_arg, "/F"])?,
        "schtasks /Create",
    )?;
    let _ = fs::remove_file(&xml_file); // temp: cleanup is best-effort
    expect_ok(
        schtasks_output(&["/Run", "/TN", TASK_NAME])?,
        "schtasks /Run",
    )
}

/// Ends the task (best-effort) and deletes it. `data/`, config and log stay.
pub fn uninstall() -> Result<(), String> {
    if !is_elevated() {
        return Err("run from an elevated terminal".to_string());
    }
    let _ = schtasks_output(&["/End", "/TN", TASK_NAME]);
    expect_ok(
        schtasks_output(&["/Delete", "/TN", TASK_NAME, "/F"])?,
        "schtasks /Delete",
    )
}

/// Copies the running `pcw-agent.exe` and its sibling `pcw-dash.exe` into
/// `<data_dir>\bin\`. Task 15 may not have produced the dashboard yet:
/// a missing sibling is a note on the console, NOT an install failure — only
/// the agent exe itself must be locatable (it is the running image).
fn install_bin_exes() -> Result<PathBuf, String> {
    let data_dir = pcw_config::default_data_dir().map_err(|e| format!("no data directory: {e}"))?;
    let bin_dir = data_dir.join("bin");
    fs::create_dir_all(&bin_dir).map_err(|e| format!("creating {}: {e}", bin_dir.display()))?;
    let here = env::current_exe().map_err(|e| format!("cannot locate pcw-agent.exe: {e}"))?;
    let dash = here.with_file_name("pcw-dash.exe");
    fs::copy(&here, bin_dir.join("pcw-agent.exe"))
        .map_err(|e| format!("copying pcw-agent.exe to {}: {e}", bin_dir.display()))?;
    if dash.exists() {
        fs::copy(&dash, bin_dir.join("pcw-dash.exe"))
            .map_err(|e| format!("copying pcw-dash.exe to {}: {e}", bin_dir.display()))?;
    } else {
        println!(
            "note: sibling pcw-dash.exe not found next to pcw-agent.exe; install continues without it"
        );
    }
    Ok(bin_dir)
}

/// `DOMAIN\user` of the installing account (the task's logon trigger needs it).
fn user_name() -> Result<String, String> {
    let user = env::var("USERNAME")
        .map_err(|_| "USERNAME not set: cannot determine the installing user".to_string())?;
    Ok(match env::var("USERDOMAIN") {
        Ok(domain) => format!(r"{domain}\{user}"),
        Err(_) => user,
    })
}

/// The task XML as a UTF-16 LE BOM file in the system temp dir, deleted by
/// [`install`] right after `/Create`.
fn write_temp_xml(xml: &str) -> Result<PathBuf, String> {
    let path = env::temp_dir().join(format!("pcw-agent-task-{}.xml", std::process::id()));
    fs::write(&path, utf16_le_bom(xml)).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path)
}

/// Runs `schtasks <args>` and captures its output.
fn schtasks_output(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new(SCHTASKS)
        .args(args)
        .output()
        .map_err(|e| format!("running schtasks: {e}"))
}

/// Non-zero exit ⇒ `Err(stderr)` (spec §6; stderr/stdout often in OEM code
/// page, so lossy conversion is right for diagnostics).
fn expect_ok(output: std::process::Output, what: &str) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let lost = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if lost.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        lost
    };
    Err(format!("{what} failed: {detail}"))
}
