//! Temporary diagnostics: which difference between a check's AppContainer launch (whose cmd
//! starts programs) and an agent's (whose cmd cannot) blocks child processes.
#![cfg(windows)]

use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
use rappct::{AppContainerProfile, SecurityCapabilitiesBuilder};
use std::ffi::OsString;
use std::io::Read;

#[test]
fn which_launches_can_start_programs() {
    let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
    let dir = match std::env::var("KT_DIAG_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => std::env::temp_dir().join(format!("kt-diag-{}", std::process::id())),
    };
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    // As a check's: the Windows essentials, TEMP in a granted directory, sorted.
    let base = ["PATH", "PATHEXT", "SystemRoot", "windir", "SystemDrive", "COMSPEC", "USERPROFILE", "LOCALAPPDATA", "HOMEDRIVE", "HOMEPATH", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE"];
    let kintrel = std::path::PathBuf::from(env!("CARGO_BIN_EXE_kintrel"));
    std::fs::copy(&kintrel, dir.join("k.exe")).unwrap();
    std::fs::copy(sys.join("findstr.exe"), dir.join("f.exe")).unwrap();
    std::fs::write(dir.join("t.txt"), "hello\n").unwrap();
    let programs = [
        ("findstr (bare name)", "findstr hello t.txt".to_string()),
        ("findstr.exe (bare name)", "findstr.exe hello t.txt".to_string()),
        ("full path, PATH's spelling", "C:\\Windows\\system32\\findstr.exe hello t.txt".to_string()),
        ("full path via %SystemRoot%", "%SystemRoot%\\System32\\findstr.exe hello t.txt".to_string()),
        ("copy in cwd, bare name", "f.exe hello t.txt".to_string()),
        ("copy in cwd, .\\ prefix", ".\\f.exe hello t.txt".to_string()),
        ("kintrel copy in cwd, bare name", "k.exe --version".to_string()),
        ("whoami (bare name)", "whoami".to_string()),
    ];
    let mut report = String::new();
    let profile = AppContainerProfile::ensure(&format!("kintrel.diag.{}", std::process::id()), "Kintrel diag", Some("diag")).unwrap();
    std::process::Command::new("icacls").arg(&dir).args(["/grant", &format!("*{}:(OI)(CI)M", profile.sid.as_string()), "/T", "/C", "/Q"]).status().ok();
    let mut env: Vec<(OsString, OsString)> = base.iter().filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v))).collect();
    env.extend(["TEMP", "TMP"].map(|k| (OsString::from(k), dir.join("tmp").into_os_string())));
    env.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let sec = SecurityCapabilitiesBuilder::new(&profile.sid).with_named(&["kintrelChecks"]).build().unwrap();
    for (name, run) in &programs {
        let opts = LaunchOptions {
            exe: sys.join("cmd.exe"),
            cmdline: Some(format!(" /D /S /C \"({run}) 2>&1\"")),
            cwd: Some(dir.clone()),
            env: Some(env.clone()),
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
                let code = io.wait(Some(std::time::Duration::from_secs(10)));
                format!("exit {code:?}: {}", out.replace("\r\n", " | "))
            }
            Err(e) => format!("launch failed: {e}"),
        };
        report.push_str(&format!("{name}: {line}\n"));
    }
    profile.delete().ok();
    // The same programs through the check sandbox's own launcher.
    let policy = kintrel_ci::sandbox::Policy::default();
    let env: Vec<(String, OsString)> = env.iter().map(|(k, v)| (k.to_string_lossy().into_owned(), v.clone())).collect();
    for (name, run) in &programs {
        let line = match kintrel_ci::sandbox::start(&format!("({run}) 2>&1"), Some(&dir), Some(&policy), &env) {
            Ok(mut r) => {
                let mut out = String::new();
                r.output.read_to_string(&mut out).ok();
                format!("ok={:?}: {}", r.wait(std::time::Duration::from_secs(10)), out.replace("\r\n", " | "))
            }
            Err(e) => format!("start failed: {e}"),
        };
        report.push_str(&format!("[sandbox::start] {name}: {line}\n"));
    }
    panic!("diagnostics:\n{report}");
}
