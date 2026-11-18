//! Asking REA what a recorded function is. Static analysis of the binary on disk, one function at
//! a time, in a background process: the first request for a binary can take minutes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use object::{Object, ObjectSegment};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    pub code_id: Option<String>,
    pub rva: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Explanation {
    pub name: String,
    pub address: String,
    pub pseudocode: String,
    pub callers: Vec<String>,
    pub callees: Vec<String>,
    /// What this analysis could not report, in REA's words.
    pub missing: Vec<String>,
}

/// The address REA knows the function by, after proving `bytes` is the binary that was recorded.
/// A rebuilt program can hold a different function at the same offset.
pub fn entry_address(bytes: &[u8], code_id: Option<&str>, rva: u64) -> Result<u64, String> {
    if matches!(bytes.get(..4), Some([0xca, 0xfe, 0xba, 0xbe | 0xbf] | [0xbe | 0xbf, 0xba, 0xfe, 0xca])) {
        return Err("This is a universal binary, which the Kuna engine does not accept.".into());
    }
    let file = object::File::parse(bytes).map_err(|e| format!("This binary could not be read: {e}."))?;
    let uuid = file.mach_uuid().ok().flatten().ok_or("This binary has no ID to compare with the recording.")?;
    let actual: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    let wanted = code_id.ok_or("The recording has no ID for this binary, so a match cannot be confirmed.")?;
    if !actual.eq_ignore_ascii_case(&wanted.replace('-', "")) {
        return Err("This binary changed after the recording. Record the program again.".into());
    }
    let text = file
        .segments()
        .find(|s| s.name().ok().flatten() == Some("__TEXT"))
        .ok_or("This binary has no code segment.")?;
    Ok(text.address() + rva)
}

fn locate(request: &Request) -> Result<u64, String> {
    let bytes = std::fs::read(&request.path).map_err(|e| {
        let system = ["/usr/lib/", "/System/"].iter().any(|p| request.path.starts_with(p));
        if system && e.kind() == std::io::ErrorKind::NotFound {
            "This is a macOS system library. It lives in a shared cache, with no separate file to analyze.".to_string()
        } else {
            format!("The recorded binary could not be opened at {}: {e}.", request.path)
        }
    })?;
    entry_address(&bytes, request.code_id.as_deref(), request.rva)
}

fn names(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        if let Some(name) = item["name"].as_str().or(item["address"].as_str()) {
            // Darwin adds one underscore to Rust's `_R` symbols, which hides them from the demangler.
            let plain = format!("{:#}", rustc_demangle::demangle(name.strip_prefix("__R").map_or(name, |_| &name[1..])));
            if !out.contains(&plain) {
                out.push(plain);
            }
        }
    }
    out
}

/// Reads the JSON `rea function` prints, for a result or for its error.
pub fn parse_output(stdout: &[u8]) -> Result<Explanation, String> {
    let v: Value = serde_json::from_slice(stdout).map_err(|_| "REA did not return a readable result.".to_string())?;
    if v.get("error").is_some() {
        return Err(v["message"].as_str().or(v["error"].as_str()).unwrap_or("REA reported an error.").to_string());
    }
    let n = &v["normalized_result"];
    if !n.is_object() {
        return Err("REA's result had no function analysis in it.".into());
    }
    let mut missing: Vec<String> = n["facet_availability"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, f)| f["available"] == Value::Bool(false))
        .map(|(facet, _)| match facet.as_str() {
            "cfg" => "control flow".to_string(),
            "strings_names" => "referenced strings".to_string(),
            "references" => "typed references".to_string(),
            other => other.replace('_', " "),
        })
        .collect();
    missing.sort();
    Ok(Explanation {
        name: n["procedure"]["name"].as_str().unwrap_or("unknown function").to_string(),
        address: n["procedure"]["address"].as_str().unwrap_or_default().to_string(),
        pseudocode: n["pseudocode"].as_str().unwrap_or_default().to_string(),
        callers: names(&n["callers"]),
        callees: names(&n["callees"]),
        missing,
    })
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The home-folder Node install comes first: an older system Node may be earlier on `PATH`.
fn find_rea() -> Option<PathBuf> {
    let own = home()?.join(".local/share/node/current/bin/rea");
    if own.is_file() { Some(own) } else { on_path("rea") }
}

fn find_kuna() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("REA_KUNA_COMMAND") {
        return Some(PathBuf::from(p));
    }
    let own = home()?.join(".local/share/kuna/v1.990/kuna-v1.990-macos-arm64/kuna");
    if own.is_file() { Some(own) } else { on_path("kuna") }
}

fn run(request: &Request, pid: &AtomicU32) -> Result<Explanation, String> {
    let address = locate(request)?;
    let rea = find_rea().ok_or("REA is not installed.")?;
    let kuna = find_kuna().ok_or("The Kuna analysis engine is not installed.")?;
    let mut path = vec![rea.parent().unwrap_or(Path::new("/")).to_path_buf()];
    path.extend(std::env::var_os("PATH").iter().flat_map(std::env::split_paths));
    let mut command = Command::new(&rea);
    command
        .args(["function", &request.path, &format!("{address:#x}"), "--provider", "kuna", "--format", "json"])
        .env("PATH", std::env::join_paths(path).map_err(|e| e.to_string())?)
        .env("REA_KUNA_COMMAND", kuna)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn().map_err(|e| format!("REA could not be started: {e}."))?;
    pid.store(child.id(), Ordering::SeqCst);
    let output = child.wait_with_output();
    pid.store(0, Ordering::SeqCst);
    let output = output.map_err(|e| format!("REA stopped unexpectedly: {e}."))?;
    parse_output(&output.stdout).map_err(|message| {
        let detail = String::from_utf8_lossy(&output.stderr);
        match detail.lines().rfind(|l| !l.trim().is_empty()) {
            Some(last) if output.stdout.is_empty() => format!("{message} {last}"),
            _ => message,
        }
    })
}

/// One running analysis. Dropping it stops the analysis and the engine under it.
pub struct Job {
    result: Mutex<Receiver<Result<Explanation, String>>>,
    pid: Arc<AtomicU32>,
}

impl Job {
    pub fn start(request: Request) -> Self {
        let (tx, result) = mpsc::channel();
        let pid = Arc::new(AtomicU32::new(0));
        let shared = pid.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run(&request, &shared));
        });
        Job { result: Mutex::new(result), pid }
    }

    pub fn poll(&self) -> Option<Result<Explanation, String>> {
        match self.result.lock().ok()?.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("The analysis stopped unexpectedly.".into())),
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let pid = self.pid.swap(0, Ordering::SeqCst);
        if pid != 0 {
            let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).stderr(Stdio::null()).status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_keeps_the_parts_the_panel_shows() {
        let out = br#"{"normalized_result":{
            "procedure":{"name":"app::load","address":"0x100000010"},
            "pseudocode":"void load() {}",
            "callers":[{"name":"main"},{"name":"main"}],
            "callees":[{"address":"0x100000020","name":null},{"name":"__RNvCs1234_7mycrate3foo"}],
            "facet_availability":{"pseudocode":{"available":true},"cfg":{"available":false,"reason":"No blocks."}}}}"#;
        let e = parse_output(out).unwrap();
        assert_eq!(e.name, "app::load");
        assert_eq!(e.callers, ["main"]);
        assert_eq!(e.callees, ["0x100000020", "mycrate::foo"]);
        assert_eq!(e.missing, ["control flow"]);
    }

    #[test]
    fn an_error_says_what_rea_said() {
        let e = parse_output(br#"{"error":"Analysis failed","message":"not a procedure entry"}"#).unwrap_err();
        assert_eq!(e, "not a procedure entry");
    }

    #[test]
    fn a_universal_binary_is_refused_before_parsing() {
        assert!(entry_address(&[0xca, 0xfe, 0xba, 0xbe, 0, 0], Some("x"), 0).unwrap_err().contains("universal"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_binary_must_be_the_one_recorded() {
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let file = object::File::parse(&*bytes).unwrap();
        let id: String = file.mach_uuid().unwrap().unwrap().iter().map(|b| format!("{b:02X}")).collect();
        let base = file.segments().find(|s| s.name().ok().flatten() == Some("__TEXT")).unwrap().address();
        assert_eq!(entry_address(&bytes, Some(&id), 0x20).unwrap(), base + 0x20);
        assert!(entry_address(&bytes, Some("00000000000000000000000000000000"), 0x20).unwrap_err().contains("changed"));
    }
}
