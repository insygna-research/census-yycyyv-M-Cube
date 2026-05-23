#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use image as image_rs;
use serde_json::{json, Value};
use sysinfo::System;
use tauri::image::Image;
use tauri::{Manager, RunEvent};

struct BackendProcessState(Mutex<Option<Child>>);

struct SidecarRuntime {
    child: Child,
    log_file: PathBuf,
}

struct CliOptions {
    workflow: Option<String>,
    input: Option<String>,
    timeout_sec: u64,
    pretty: bool,
    show_help: bool,
    show_version: bool,
}

fn append_log(log_file: &Path, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_file) {
        let _ = writeln!(f, "{msg}");
    }
}

fn set_main_window_icon(app: &tauri::App) {
    let icon_bytes = include_bytes!("../icons/icon.png");
    if let Ok(decoded) = image_rs::load_from_memory(icon_bytes) {
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        let icon = Image::new_owned(rgba.into_raw(), width, height);
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.set_icon(icon);
        }
    }
}

fn resolve_runtime_root_for_cli() -> PathBuf {
    if let Ok(app_data) = std::env::var("APPDATA") {
        let base = PathBuf::from(app_data).join("M-Cube").join("runtime");
        let _ = fs::create_dir_all(&base);
        return base;
    }
    let base = std::env::temp_dir().join("mcube-runtime");
    let _ = fs::create_dir_all(&base);
    base
}

#[cfg(target_os = "macos")]
fn configure_main_window_platform(app: &tauri::App) {
    // On macOS, use the native traffic-light buttons via titleBarStyle: Overlay,
    // so re-enable decorations (the JSON config sets decorations=false for Windows/Linux).
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_decorations(true);
    }
}

#[cfg(not(target_os = "macos"))]
fn configure_main_window_platform(_app: &tauri::App) {
    // Windows/Linux: keep frameless window with custom titlebar (decorations=false from config).
}

fn resolve_log_file(app_handle: &tauri::AppHandle) -> PathBuf {
    if let Ok(app_data_dir) = app_handle.path().app_data_dir() {
        let log_dir = app_data_dir.join("runtime").join("logs");
        let _ = fs::create_dir_all(&log_dir);
        return log_dir.join("backend-sidecar.log");
    }
    std::env::temp_dir().join("mcube-backend-sidecar.log")
}

fn sidecar_binary_name() -> String {
    if cfg!(target_os = "windows") {
        format!("mcube-backend-{}.exe", env!("TAURI_ENV_TARGET_TRIPLE"))
    } else {
        format!("mcube-backend-{}", env!("TAURI_ENV_TARGET_TRIPLE"))
    }
}

fn find_sidecar_binary() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?.to_path_buf();
    let sidecar_name = sidecar_binary_name();

    let mut roots: Vec<PathBuf> = vec![exe_dir.clone()];
    for ancestor in exe_dir.ancestors().take(5) {
        roots.push(ancestor.to_path_buf());
    }

    for root in roots {
        let candidates = [
            root.join("binaries"),
            root.join("resources").join("binaries"),
            root.join("Resources").join("binaries"),
            root.join("lib").join("mcube").join("binaries"),
            root.join("_up_").join("resources").join("binaries"),
        ];
        for binaries_dir in candidates {
            let sidecar_path = binaries_dir.join(&sidecar_name);
            if sidecar_path.exists() {
                return Some((binaries_dir, sidecar_path));
            }
        }
    }
    None
}

fn spawn_sidecar(runtime_root: &Path, log_file: &Path) -> Result<SidecarRuntime, String> {
    let uploads_root = runtime_root.join("uploads");
    let sidecar_tmp = std::env::temp_dir().join("mcube_sidecar_tmp");
    fs::create_dir_all(&uploads_root).map_err(|e| format!("Failed to create uploads root: {e}"))?;
    fs::create_dir_all(&sidecar_tmp).map_err(|e| format!("Failed to create temp root: {e}"))?;

    let (binaries_dir, sidecar_path) = find_sidecar_binary().ok_or_else(|| {
        String::from("Backend sidecar binary not found. Expected bundled binaries directory.")
    })?;

    let stdout_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|e| format!("Failed to open sidecar stdout log: {e}"))?;
    let stderr_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)
        .map_err(|e| format!("Failed to open sidecar stderr log: {e}"))?;

    let mut command = Command::new(&sidecar_path);
    command
        .current_dir(&binaries_dir)
        .env("UPLOAD_ROOT_DIR", uploads_root.to_string_lossy().to_string())
        .env("MCUBE_BACKEND_HOST", "127.0.0.1")
        .env("MCUBE_BACKEND_PORT", "8000")
        .env("TMP", sidecar_tmp.to_string_lossy().to_string())
        .env("TEMP", sidecar_tmp.to_string_lossy().to_string())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let child = command.spawn().map_err(|e| {
        format!(
            "Failed to spawn backend sidecar: {e}; bin={}; cwd={}",
            sidecar_path.display(),
            binaries_dir.display()
        )
    })?;

    Ok(SidecarRuntime {
        child,
        log_file: log_file.to_path_buf(),
    })
}

fn kill_sidecar_descendants_and_leftovers(root_pid: u32, process_name_marker: &str, log_file: &Path) {
    let mut sys = System::new_all();
    sys.refresh_all();

    let self_pid = std::process::id().to_string();
    let root_pid_str = root_pid.to_string();
    let marker = process_name_marker.to_lowercase();

    let mut frontier: Vec<String> = vec![root_pid_str.clone()];
    let mut descendants: Vec<String> = Vec::new();
    loop {
        let mut discovered: Vec<String> = Vec::new();
        for (pid, process) in sys.processes() {
            let parent_str = process.parent().map(|p| p.to_string());
            if let Some(parent) = parent_str {
                let pid_str = pid.to_string();
                if frontier.iter().any(|f| f == &parent)
                    && pid_str != self_pid
                    && !descendants.iter().any(|d| d == &pid_str)
                {
                    discovered.push(pid_str);
                }
            }
        }
        if discovered.is_empty() {
            break;
        }
        frontier = discovered.clone();
        descendants.extend(discovered);
    }

    for pid_str in descendants.iter().rev() {
        for (pid, process) in sys.processes() {
            if pid.to_string() == *pid_str {
                let name = process.name().to_string();
                let killed = process.kill();
                append_log(
                    log_file,
                    &format!("Kill descendant pid={pid_str} name={name} result={killed}"),
                );
            }
        }
    }

    for (pid, process) in sys.processes() {
        let pid_str = pid.to_string();
        if pid_str == self_pid {
            continue;
        }
        let name = process.name().to_lowercase();
        if name.contains(&marker) {
            let killed = process.kill();
            append_log(
                log_file,
                &format!("Kill fallback pid={pid_str} name={name} result={killed}"),
            );
        }
    }
}

fn kill_stale_sidecars_by_name(process_name_marker: &str, log_file: &Path) {
    let mut sys = System::new_all();
    sys.refresh_all();

    let self_pid = std::process::id().to_string();
    let marker = process_name_marker.to_lowercase();

    for (pid, process) in sys.processes() {
        let pid_str = pid.to_string();
        if pid_str == self_pid {
            continue;
        }
        let name = process.name().to_lowercase();
        if name.contains(&marker) {
            let killed = process.kill();
            append_log(
                log_file,
                &format!("Startup sweep kill pid={pid_str} name={name} result={killed}"),
            );
        }
    }
}

fn shutdown_sidecar(child: &mut Child, log_file: &Path) {
    let root_pid = child.id();
    let _ = child.kill();
    append_log(
        log_file,
        &format!("Primary sidecar kill sent. root_pid={root_pid}"),
    );
    kill_sidecar_descendants_and_leftovers(root_pid, "mcube-backend", log_file);
    append_log(log_file, "Sidecar cleanup completed.");
}

fn parse_cli_args(args: &[String]) -> Result<CliOptions, String> {
    let mut workflow: Option<String> = None;
    let mut input: Option<String> = None;
    let mut timeout_sec: u64 = 120;
    let mut pretty = false;
    let mut show_help = false;
    let mut show_version = false;

    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--help" | "-h" => show_help = true,
            "--version" | "-v" => show_version = true,
            "--pretty" => pretty = true,
            "--workflow" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| String::from("Missing value for --workflow"))?;
                workflow = Some(value.to_string());
            }
            "--input" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| String::from("Missing value for --input"))?;
                input = Some(value.to_string());
            }
            "--timeout-sec" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| String::from("Missing value for --timeout-sec"))?;
                timeout_sec = value.parse::<u64>().map_err(|_| String::from("Invalid integer for --timeout-sec"))?;
            }
            "--cli" => {}
            _ => {
                return Err(format!("Unknown argument: {arg}"));
            }
        }
        i += 1;
    }

    Ok(CliOptions {
        workflow,
        input,
        timeout_sec,
        pretty,
        show_help,
        show_version,
    })
}

fn cli_help_text() -> &'static str {
    "M-Cube CLI\n\nUsage:\n  M-Cube.exe --cli --workflow <draft|oa|compare|polish> --input <json|@file> [--timeout-sec N] [--pretty]\n  M-Cube.exe --cli --help\n  M-Cube.exe --cli --version\n"
}

fn read_input_json(input_arg: &str) -> Result<Value, String> {
    if let Some(path) = input_arg.strip_prefix('@') {
        let content = fs::read_to_string(path).map_err(|e| format!("Failed to read input file: {e}"))?;
        serde_json::from_str::<Value>(&content).map_err(|e| format!("Invalid JSON in input file: {e}"))
    } else {
        serde_json::from_str::<Value>(input_arg).map_err(|e| format!("Invalid input JSON: {e}"))
    }
}

fn wait_for_api_ready(base_url: &str, timeout_sec: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_sec.max(1));
    let health_url = format!("{base_url}/openapi.json");
    loop {
        if Instant::now() > deadline {
            return Err(String::from("Backend API did not become ready within timeout"));
        }
        match ureq::get(&health_url).call() {
            Ok(resp) if (200..300).contains(&resp.status()) => return Ok(()),
            _ => sleep(Duration::from_millis(250)),
        }
    }
}

fn post_json(base_url: &str, path: &str, body: &Value, timeout_sec: u64) -> Result<Value, String> {
    let url = format!("{base_url}{path}");
    let timeout = Duration::from_secs(timeout_sec.max(1));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();
    let req = agent.post(&url).set("Content-Type", "application/json");

    match req.send_string(&body.to_string()) {
        Ok(resp) => {
            let text = resp
                .into_string()
                .map_err(|e| format!("Failed to read API response body: {e}"))?;
            serde_json::from_str::<Value>(&text).map_err(|e| format!("Failed to parse API JSON response: {e}"))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let body_text = resp.into_string().unwrap_or_default();
            Err(format!("API status={status}, body={body_text}"))
        }
        Err(e) => Err(format!("HTTP request failed: {e}")),
    }
}

fn maybe_continue_draft(base_url: &str, start_resp: &Value, timeout_sec: u64) -> Result<Value, String> {
    let status = start_resp
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status != "waiting_human" {
        return Ok(start_resp.clone());
    }
    let session_id = start_resp
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| String::from("draft/start returned waiting_human but missing session_id"))?;
    let claims = start_resp
        .get("data")
        .and_then(|v| v.get("claims"))
        .cloned()
        .ok_or_else(|| String::from("draft/start returned waiting_human but missing data.claims"))?;

    let continue_payload = json!({
        "session_id": session_id,
        "approved_claims": claims
    });
    post_json(base_url, "/api/v1/draft/continue", &continue_payload, timeout_sec)
}

fn execute_cli_task(opts: &CliOptions) -> Result<Value, String> {
    if opts.show_help {
        return Ok(json!({ "ok": true, "help": cli_help_text() }));
    }
    if opts.show_version {
        return Ok(json!({
            "ok": true,
            "product": "M-Cube",
            "version": env!("CARGO_PKG_VERSION")
        }));
    }

    let workflow = opts
        .workflow
        .as_deref()
        .ok_or_else(|| String::from("Missing required argument: --workflow"))?;
    let input_arg = opts
        .input
        .as_deref()
        .ok_or_else(|| String::from("Missing required argument: --input"))?;
    let input_json = read_input_json(input_arg)?;

    let runtime_root = resolve_runtime_root_for_cli();
    let log_dir = runtime_root.join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| format!("Failed to create CLI log directory: {e}"))?;
    let log_file = log_dir.join("backend-sidecar.log");

    append_log(
        &log_file,
        &format!("=== Launch M-Cube backend sidecar (CLI) at {:?} ===", std::time::SystemTime::now()),
    );
    kill_stale_sidecars_by_name("mcube-backend", &log_file);
    let mut runtime = spawn_sidecar(&runtime_root, &log_file)?;
    append_log(&log_file, "Backend sidecar spawned (CLI mode).");

    let result = (|| {
        let base_url = "http://127.0.0.1:8000";
        wait_for_api_ready(base_url, opts.timeout_sec)?;
        let response = match workflow {
            "draft" => {
                let start_resp = post_json(base_url, "/api/v1/draft/start", &input_json, opts.timeout_sec)?;
                maybe_continue_draft(base_url, &start_resp, opts.timeout_sec)?
            }
            "oa" => post_json(base_url, "/api/v1/oa/start", &input_json, opts.timeout_sec)?,
            "compare" => post_json(base_url, "/api/v1/compare/start", &input_json, opts.timeout_sec)?,
            "polish" => post_json(base_url, "/api/v1/polish/start", &input_json, opts.timeout_sec)?,
            _ => return Err(format!("Unsupported workflow: {workflow}")),
        };
        Ok(response)
    })();

    shutdown_sidecar(&mut runtime.child, &runtime.log_file);
    result
}

fn print_json_stdout(value: &Value, pretty: bool) {
    if pretty {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
        );
    } else {
        println!("{value}");
    }
}

fn run_cli_mode() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_cli_args(&args) {
        Ok(v) => v,
        Err(e) => {
            print_json_stdout(
                &json!({
                    "ok": false,
                    "error": { "code": "CLI_INVALID_ARGUMENT", "message": e },
                    "help": cli_help_text(),
                }),
                false,
            );
            return 2;
        }
    };

    match execute_cli_task(&opts) {
        Ok(payload) => {
            print_json_stdout(
                &json!({
                    "ok": true,
                    "workflow": opts.workflow,
                    "result": payload
                }),
                opts.pretty,
            );
            0
        }
        Err(err) => {
            print_json_stdout(
                &json!({
                    "ok": false,
                    "error": { "code": "CLI_EXECUTION_FAILED", "message": err }
                }),
                opts.pretty,
            );
            3
        }
    }
}

fn run_gui_mode() {
    let app = tauri::Builder::default()
        .manage(BackendProcessState(Mutex::new(None)))
        .setup(|app| {
            set_main_window_icon(app);
            configure_main_window_platform(app);

            if cfg!(debug_assertions) {
                return Ok(());
            }

            let app_handle = app.handle().clone();
            let app_data_dir = app_handle
                .path()
                .app_data_dir()
                .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
            fs::create_dir_all(&app_data_dir).map_err(|e| format!("Failed to create app data directory: {e}"))?;
            let runtime_root = app_data_dir.join("runtime");
            let log_dir = runtime_root.join("logs");
            fs::create_dir_all(&log_dir).map_err(|e| format!("Failed to create log directory: {e}"))?;
            let log_file = log_dir.join("backend-sidecar.log");

            append_log(
                &log_file,
                &format!("=== Launch M-Cube backend sidecar at {:?} ===", std::time::SystemTime::now()),
            );
            kill_stale_sidecars_by_name("mcube-backend", &log_file);

            match spawn_sidecar(&runtime_root, &log_file) {
                Ok(runtime) => {
                    let state = app_handle.state::<BackendProcessState>();
                    let mut guard = state
                        .0
                        .lock()
                        .map_err(|_| String::from("Failed to lock backend process state"))?;
                    *guard = Some(runtime.child);
                    append_log(&log_file, "Backend sidecar spawned.");
                }
                Err(e) => {
                    append_log(&log_file, &e);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
            let log_file = resolve_log_file(&app_handle);
            let state = app_handle.state::<BackendProcessState>();
            let child_to_kill = match state.0.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => None,
            };
            if let Some(mut child) = child_to_kill {
                shutdown_sidecar(&mut child, &log_file);
            }
        }
    });
}

fn main() {
    let is_cli = std::env::args().any(|arg| arg == "--cli");
    if is_cli {
        let code = run_cli_mode();
        std::process::exit(code);
    }
    run_gui_mode();
}
