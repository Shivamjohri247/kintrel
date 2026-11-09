//! Temporary diagnostics: which AppContainer launch settings let cmd start a program.
#![cfg(windows)]

use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
use rappct::{AppContainerProfile, KnownCapability, SecurityCapabilitiesBuilder};
use std::ffi::OsString;
use std::io::Read;

#[test]
#[allow(clippy::type_complexity)]
fn which_launches_can_start_programs() {
    let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
    let dir = std::env::temp_dir().join(format!("kt-diag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let minimal: Vec<(OsString, OsString)> = ["SystemRoot", "windir", "PATH", "PATHEXT", "COMSPEC", "SystemDrive"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
        .collect();
    let mut full: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    full.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut report = String::new();
    let cases: Vec<(&str, Vec<(OsString, OsString)>, Vec<KnownCapability>, Vec<&str>, Option<std::path::PathBuf>, bool)> = vec![
        ("minimal env, no caps, cwd sys", minimal.clone(), vec![], vec![], Some(sys.clone()), true),
        ("full env, no caps, cwd sys", full.clone(), vec![], vec![], Some(sys.clone()), true),
        ("minimal env, internet", minimal.clone(), vec![KnownCapability::InternetClient], vec![], Some(sys.clone()), true),
        ("minimal env, named cap", minimal.clone(), vec![], vec!["kintrelAgents.diag"], Some(sys.clone()), true),
        ("minimal env, cwd granted dir", minimal.clone(), vec![], vec![], Some(dir.clone()), true),
        ("minimal env, no job", minimal.clone(), vec![], vec![], Some(sys.clone()), false),
        ("full env, internet, named, no job", full.clone(), vec![KnownCapability::InternetClient], vec!["kintrelAgents.diag"], Some(sys.clone()), false),
    ];
    for (i, (name, env, known, named, cwd, job)) in cases.into_iter().enumerate() {
        let profile = AppContainerProfile::ensure(&format!("kintrel.diag.{}.{i}", std::process::id()), "diag", None).unwrap();
        std::process::Command::new("icacls").arg(&dir).args(["/grant", &format!("*{}:(OI)(CI)M", profile.sid.as_string()), "/T", "/C", "/Q"]).status().ok();
        let sec = SecurityCapabilitiesBuilder::new(&profile.sid).with_known(&known).with_named(&named).build().unwrap();
        let opts = LaunchOptions {
            exe: sys.join("cmd.exe"),
            cmdline: Some(format!(" /D /S /C \"{}\\whoami.exe 2>&1 & echo exit=%errorlevel%\"", sys.display())),
            cwd,
            env: Some(env),
            stdio: StdioConfig::Pipe,
            join_job: job.then(|| JobLimits { kill_on_job_close: true, ..Default::default() }),
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
        report.push_str(&format!("{name}: {line}\n"));
        profile.delete().ok();
    }
    panic!("diagnostics:\n{report}");
}
