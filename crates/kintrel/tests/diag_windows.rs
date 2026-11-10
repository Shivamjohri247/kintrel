//! Temporary diagnostics: which difference between a check's AppContainer launch (whose cmd
//! starts programs) and an agent's (whose cmd cannot) blocks child processes.
#![cfg(windows)]

use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
use rappct::{AppContainerProfile, KnownCapability, SecurityCapabilitiesBuilder};
use std::ffi::OsString;
use std::io::Read;

struct Case {
    name: &'static str,
    full_env: bool,
    internet: bool,
    named: &'static str,
    argv0: bool,
}

#[test]
fn which_launches_can_start_programs() {
    let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
    let dir = std::env::temp_dir().join(format!("kt-diag-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    // As a check's: the Windows essentials, TEMP in a granted directory, sorted.
    let base = ["PATH", "PATHEXT", "SystemRoot", "windir", "SystemDrive", "COMSPEC", "USERPROFILE", "LOCALAPPDATA", "HOMEDRIVE", "HOMEPATH", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE"];
    let cases = [
        Case { name: "as a check", full_env: false, internet: false, named: "kintrelChecks", argv0: false },
        Case { name: "+ internet", full_env: false, internet: true, named: "kintrelChecks", argv0: false },
        Case { name: "per-board capability", full_env: false, internet: false, named: "kintrelAgents.diag", argv0: false },
        Case { name: "full environment", full_env: true, internet: false, named: "kintrelChecks", argv0: false },
        Case { name: "argv0 in command line", full_env: false, internet: false, named: "kintrelChecks", argv0: true },
        Case { name: "as an agent", full_env: true, internet: true, named: "kintrelAgents.diag", argv0: true },
    ];
    let mut report = String::new();
    for (i, c) in cases.iter().enumerate() {
        let profile = AppContainerProfile::ensure(&format!("kintrel.diag.{}.{i}", std::process::id()), "Kintrel diag", Some("diag")).unwrap();
        std::process::Command::new("icacls").arg(&dir).args(["/grant", &format!("*{}:(OI)(CI)M", profile.sid.as_string()), "/T", "/C", "/Q"]).status().ok();
        let mut env: Vec<(OsString, OsString)> = if c.full_env {
            std::env::vars_os().collect()
        } else {
            base.iter().filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v))).collect()
        };
        env.retain(|(k, _)| !["TEMP", "TMP"].contains(&k.to_string_lossy().to_uppercase().as_str()));
        env.extend(["TEMP", "TMP"].map(|k| (OsString::from(k), dir.join("tmp").into_os_string())));
        env.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
        let known = if c.internet { vec![KnownCapability::InternetClient] } else { vec![] };
        let sec = SecurityCapabilitiesBuilder::new(&profile.sid).with_known(&known).with_named(&[c.named]).build().unwrap();
        let script = format!("{}\\whoami.exe 2>&1 & echo exit=%errorlevel%", sys.display());
        let cmdline = if c.argv0 { format!("{}\\cmd.exe /D /S /C \"{script}\"", sys.display()) } else { format!(" /D /S /C \"{script}\"") };
        let opts = LaunchOptions {
            exe: sys.join("cmd.exe"),
            cmdline: Some(cmdline),
            cwd: Some(dir.clone()),
            env: Some(env),
            stdio: StdioConfig::Pipe,
            join_job: Some(JobLimits { kill_on_job_close: true, ..Default::default() }),
            ..Default::default()
        };
        let line = match launch_in_container_with_io(&sec, &opts) {
            Ok(mut io) => {
                let mut out = String::new();
                if let Some(mut o) = io.stdout.take() {
                    o.read_to_string(&mut out).ok();
                }
                let _ = io.wait(Some(std::time::Duration::from_secs(10)));
                out.replace("\r\n", " | ")
            }
            Err(e) => format!("launch failed: {e}"),
        };
        report.push_str(&format!("{}: {line}\n", c.name));
        profile.delete().ok();
    }
    panic!("diagnostics:\n{report}");
}
