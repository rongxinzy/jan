use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tauri::{Emitter, Runtime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

const OPENCLAW_INSTALL_PROGRESS_EVENT: &str = "openclaw-install-progress";
const OPENCLAW_GATEWAY_PORT: u16 = 18789;
const GATEWAY_READY_EVENT: &str = "openclaw-gateway-ready";
const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmjs.org";
const TEMPORARY_CHINA_NPM_REGISTRY: &str = "https://registry.npmmirror.com";

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

static GATEWAY_PID: StdMutex<Option<u32>> = StdMutex::new(None);

async fn is_port_open(host: &str, port: u16) -> bool {
    match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((host, port))).await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

async fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("tasklist");
        command.args(["/FI", &format!("PID eq {}", pid), "/NH"]);
        apply_no_window(&mut command);
        match command.output().await {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

fn set_gateway_pid(pid: u32) {
    if let Ok(mut guard) = GATEWAY_PID.lock() {
        *guard = Some(pid);
    }
}

fn take_gateway_pid() -> Option<u32> {
    GATEWAY_PID.lock().ok().and_then(|mut guard| guard.take())
}

fn current_gateway_pid() -> Option<u32> {
    GATEWAY_PID.lock().ok().and_then(|guard| *guard)
}

async fn find_pid_by_port(port: u16) -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("cmd");
        command.args([
            "/C",
            &format!(
                "for /f \"tokens=5\" %a in ('netstat -ano ^| findstr :{} ^| findstr LISTENING') do @echo %a",
                port
            ),
        ]);
        apply_no_window(&mut command);
        let output = command.output().await.ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().next()?.trim().parse().ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new("lsof");
        command.args(["-i", &format!("tcp:{}", port), "-t"]);
        let output = command.output().await.ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().next()?.trim().parse().ok()
    }
}

async fn resolve_gateway_pid_after_spawn(expected_spawn_pid: u32) {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Only update if the saved PID still matches the expected spawn PID.
        // This prevents a stale resolve task from overwriting a newer launch.
        if current_gateway_pid() != Some(expected_spawn_pid) {
            log::info!("Aborting PID resolve: spawn PID {} no longer current", expected_spawn_pid);
            return;
        }
        if let Some(pid) = find_pid_by_port(OPENCLAW_GATEWAY_PORT).await {
            if pid != expected_spawn_pid {
                log::info!(
                    "Resolved gateway listener PID {} (replacing spawn PID {}) on port {}",
                    pid,
                    expected_spawn_pid,
                    OPENCLAW_GATEWAY_PORT
                );
                set_gateway_pid(pid);
            }
            return;
        }
    }
    log::warn!(
        "Could not resolve gateway listener PID on port {} after 15s; using spawn PID {}",
        OPENCLAW_GATEWAY_PORT,
        expected_spawn_pid
    );
}

fn err_to_string<E: std::fmt::Display>(e: E) -> String {
    format!("Error: {e}")
}

fn normalize_registry(value: &str) -> String {
    value.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn is_default_npm_registry(registry: &str) -> bool {
    registry.is_empty()
        || registry == DEFAULT_NPM_REGISTRY
        || registry == "http://registry.npmjs.org"
}

fn is_known_domestic_npm_registry(registry: &str) -> bool {
    registry.contains("registry.npmmirror.com")
        || registry.contains("registry.npm.taobao.org")
        || registry.contains("registry.cnpmjs.org")
}

fn resolve_openclaw_install_registry(current_registry: Option<&str>) -> Option<&'static str> {
    let normalized = current_registry.map(normalize_registry).unwrap_or_default();
    if is_default_npm_registry(&normalized) {
        return Some(TEMPORARY_CHINA_NPM_REGISTRY);
    }

    if is_known_domestic_npm_registry(&normalized) {
        return None;
    }

    None
}

async fn get_npm_registry(npm: &std::path::Path) -> Option<String> {
    let mut command = Command::new(npm);
    command.args(["config", "get", "registry"]);
    apply_no_window(&mut command);

    match command.output().await {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() || stdout.eq_ignore_ascii_case("undefined") {
                None
            } else {
                Some(stdout)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            log::warn!(
                "Failed to read npm registry before OpenClaw install (status={}): {}",
                output.status,
                stderr
            );
            None
        }
        Err(error) => {
            log::warn!(
                "Failed to run `npm config get registry` before OpenClaw install: {}",
                error
            );
            None
        }
    }
}

fn home_dir() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "Unable to determine home directory".to_string())
}

fn openclaw_config_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join(".openclaw"))
}

fn openclaw_config_path() -> Result<PathBuf, String> {
    Ok(openclaw_config_dir()?.join("openclaw.json"))
}

fn find_openclaw_binary() -> Option<String> {
    let names = ["openclaw", "clawdbot", "openclaw.cmd", "clawdbot.cmd"];
    for name in &names {
        if let Ok(path) = which::which(name) {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn apply_no_window(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn apply_no_window(_command: &mut Command) {}

fn gateway_url(bind_host: &str, port: u16) -> String {
    let host = match bind_host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    format!("http://{host}:{port}/")
}

#[tauri::command]
pub fn check_openclaw_installed() -> Option<String> {
    find_openclaw_binary()
}

#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct OpenClawStatus {
    pub installed: bool,
    pub binary_path: Option<String>,
    pub version: Option<String>,
    pub gateway_url: Option<String>,
    pub gateway_port: u16,
    pub process_running: bool,
    pub pid: Option<u32>,
    pub port_open: bool,
    pub health: String,
    pub message: Option<String>,
}

fn openclaw_not_installed_status() -> OpenClawStatus {
    OpenClawStatus {
        installed: false,
        binary_path: None,
        version: None,
        gateway_url: None,
        gateway_port: OPENCLAW_GATEWAY_PORT,
        process_running: false,
        pid: None,
        port_open: false,
        health: "not-installed".to_string(),
        message: None,
    }
}

async fn openclaw_version(bin: &str) -> Option<String> {
    let mut command = Command::new(bin);
    command.arg("--version");
    apply_no_window(&mut command);
    let output = command.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(|line| line.trim().to_string())
}

async fn get_openclaw_status_inner() -> Result<OpenClawStatus, String> {
    let Some(bin) = find_openclaw_binary() else {
        return Ok(openclaw_not_installed_status());
    };

    let version = tokio::time::timeout(Duration::from_secs(5), openclaw_version(&bin))
        .await
        .unwrap_or(None);

    let pid = current_gateway_pid();
    let process_running = match pid {
        Some(p) => is_process_alive(p).await,
        None => false,
    };
    let port_open = is_port_open("127.0.0.1", OPENCLAW_GATEWAY_PORT).await;

    let health = if process_running && port_open {
        "running"
    } else if process_running && !port_open {
        "degraded"
    } else if !process_running && port_open {
        "degraded"
    } else {
        "stopped"
    };

    let gateway_url = if port_open {
        Some(gateway_url("127.0.0.1", OPENCLAW_GATEWAY_PORT))
    } else {
        None
    };

    let message = if health == "degraded" {
        if process_running && !port_open {
            Some("Gateway process is running but port is not responding.".to_string())
        } else if !process_running && port_open {
            Some(format!(
                "Port {} is occupied by another process.",
                OPENCLAW_GATEWAY_PORT
            ))
        } else {
            None
        }
    } else {
        None
    };

    Ok(OpenClawStatus {
        installed: true,
        binary_path: Some(bin),
        version,
        gateway_url,
        gateway_port: OPENCLAW_GATEWAY_PORT,
        process_running,
        pid,
        port_open,
        health: health.to_string(),
        message,
    })
}

async fn wait_for_openclaw_status<F>(
    max_attempts: usize,
    predicate: F,
) -> Result<OpenClawStatus, String>
where
    F: Fn(&OpenClawStatus) -> bool,
{
    let mut last_status: Option<OpenClawStatus> = None;

    for _ in 0..max_attempts {
        let status = get_openclaw_status_inner().await?;
        if predicate(&status) {
            return Ok(status);
        }
        last_status = Some(status);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let detail = last_status
        .and_then(|status| status.message)
        .unwrap_or_else(|| {
            "Timed out while waiting for OpenClaw state reconciliation.".to_string()
        });

    Err(detail)
}

#[tauri::command]
pub async fn get_openclaw_status() -> Result<OpenClawStatus, String> {
    get_openclaw_status_inner().await
}

#[tauri::command]
pub async fn install_openclaw<R: Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    log::info!("Installing OpenClaw via npm...");

    app.emit(
        OPENCLAW_INSTALL_PROGRESS_EVENT,
        serde_json::json!({
            "status": "installing",
            "progress": 0.0,
            "message": "正在安装 OpenClaw，请稍候..."
        }),
    )
    .ok();

    let npm = which::which("npm")
        .map_err(|_| "npm not found on PATH. Please install Node.js first.".to_string())?;
    let current_registry = get_npm_registry(&npm).await;
    let temporary_registry = resolve_openclaw_install_registry(current_registry.as_deref());

    if let Some(registry) = temporary_registry {
        log::info!(
            "OpenClaw install will use temporary npm registry override: {} (detected current registry: {:?})",
            registry,
            current_registry
        );
        app.emit(
            OPENCLAW_INSTALL_PROGRESS_EVENT,
            serde_json::json!({
                "status": "installing",
                "progress": 8.0,
                "message": "检测到 npm 仍在使用默认源，本次安装将临时切换到国内镜像。"
            }),
        )
        .ok();
    } else {
        log::info!(
            "OpenClaw install will use existing npm registry: {:?}",
            current_registry
        );
    }

    #[cfg(target_os = "windows")]
    {
        let mut child = Command::new(&npm);
        child.args(["install", "-g", "openclaw@latest"]);
        if let Some(registry) = temporary_registry {
            child.env("npm_config_registry", registry);
            child.env("NPM_CONFIG_REGISTRY", registry);
        }
        child
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        apply_no_window(&mut child);

        let mut child = child
            .spawn()
            .map_err(|e| format!("Failed to spawn npm install: {e}"))?;

        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("npm stdout: {}", line);
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|e| format!("npm install process failed: {e}"))?;

        if !status.success() {
            return Err("npm install -g openclaw@latest failed".to_string());
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new(&npm);
        command.args(["install", "-g", "openclaw@latest"]);
        if let Some(registry) = temporary_registry {
            command.env("npm_config_registry", registry);
            command.env("NPM_CONFIG_REGISTRY", registry);
        }
        apply_no_window(&mut command);
        let output = command
            .output()
            .await
            .map_err(|e| format!("Failed to run npm install: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("npm install failed: {stderr}"));
        }
    }

    app.emit(
        OPENCLAW_INSTALL_PROGRESS_EVENT,
        serde_json::json!({
            "status": "completed",
            "progress": 100.0,
            "message": "OpenClaw 安装成功。"
        }),
    )
    .ok();

    log::info!("OpenClaw installed successfully");
    Ok(())
}

fn write_openclaw_config(model: &str) -> Result<(), String> {
    let config_dir = openclaw_config_dir()?;
    std::fs::create_dir_all(&config_dir).map_err(err_to_string)?;

    let config_path = openclaw_config_path()?;

    let mut config: serde_json::Map<String, serde_json::Value> = if config_path.exists() {
        let data = std::fs::read_to_string(&config_path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    let model_entry = serde_json::json!({
        "id": model,
        "name": model,
        "input": ["text"],
        "cost": {
            "input": 0,
            "output": 0,
            "cacheRead": 0,
            "cacheWrite": 0
        }
    });

    if !config.contains_key("models") {
        config.insert("models".to_string(), serde_json::json!({}));
    }
    let models_section = config["models"].as_object_mut().unwrap();

    if !models_section.contains_key("providers") {
        models_section.insert("providers".to_string(), serde_json::json!({}));
    }
    let providers = models_section["providers"].as_object_mut().unwrap();

    providers.insert(
        "ollama".to_string(),
        serde_json::json!({
            "api": "ollama",
            "apiKey": "ollama-local",
            "baseUrl": "http://127.0.0.1:11434",
            "models": [model_entry]
        }),
    );

    if !config.contains_key("agents") {
        config.insert("agents".to_string(), serde_json::json!({}));
    }
    let agents = config["agents"].as_object_mut().unwrap();

    if !agents.contains_key("defaults") {
        agents.insert("defaults".to_string(), serde_json::json!({}));
    }
    let defaults = agents["defaults"].as_object_mut().unwrap();

    if !defaults.contains_key("model") {
        defaults.insert("model".to_string(), serde_json::json!({}));
    }
    let model_cfg = defaults["model"].as_object_mut().unwrap();

    model_cfg.insert(
        "primary".to_string(),
        serde_json::Value::String(format!("ollama/{model}")),
    );

    let data = serde_json::to_string_pretty(&config).map_err(err_to_string)?;
    std::fs::write(&config_path, data).map_err(err_to_string)?;

    log::info!(
        "OpenClaw config written to {:?} with model {}",
        config_path,
        model
    );
    Ok(())
}

fn filtered_openclaw_env<I>(vars: I, inject_local_model: bool) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    if !inject_local_model {
        return vars.into_iter().collect();
    }

    let clear: HashSet<&str> = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "MISTRAL_API_KEY",
        "GROQ_API_KEY",
        "XAI_API_KEY",
        "OPENROUTER_API_KEY",
    ]
    .iter()
    .copied()
    .collect();

    vars.into_iter()
        .filter(|(k, _)| !clear.contains(k.as_str()))
        .collect()
}

fn openclaw_env(inject_local_model: bool) -> Vec<(String, String)> {
    filtered_openclaw_env(std::env::vars(), inject_local_model)
}

async fn spawn_gateway_foreground(bin: &str, inject_local_model: bool) -> Result<u32, String> {
    let mut command = Command::new(bin);
    command.args(["gateway", "run", "--force", "--allow-unconfigured"]);
    command.envs(openclaw_env(inject_local_model));
    apply_no_window(&mut command);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start OpenClaw gateway: {e}"))?;

    let spawn_pid = child.id().ok_or("Failed to get child PID")?;
    set_gateway_pid(spawn_pid);

    // Spawn a background task to resolve the actual listener PID via port scan.
    // On Windows the .cmd wrapper spawns cmd.exe which then spawns node.exe;
    // the spawn PID points to cmd.exe, not the real gateway process.
    tokio::spawn(async move {
        resolve_gateway_pid_after_spawn(spawn_pid).await;
    });

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::info!("[openclaw gateway stdout] {}", line);
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::warn!("[openclaw gateway stderr] {}", line);
            }
        });
    }

    Ok(spawn_pid)
}

#[tauri::command]
pub async fn launch_openclaw_gateway<R: Runtime>(
    app: tauri::AppHandle<R>,
    model: Option<String>,
) -> Result<OpenClawLaunchResult, String> {
    let inject_local_model = model.as_ref().is_some_and(|value| !value.trim().is_empty());
    log::info!(
        "Launching OpenClaw gateway (inject_local_model={}, model={:?})",
        inject_local_model,
        model
    );

    let Some(bin) = find_openclaw_binary() else {
        return Err("OpenClaw is not installed. Please install it first.".to_string());
    };

    if let Some(model) = model.as_deref().filter(|value| !value.trim().is_empty()) {
        write_openclaw_config(model)?;
    }

    // Always kill any existing gateway process first to avoid conflicts.
    stop_openclaw_gateway().await.ok();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pid = spawn_gateway_foreground(&bin, inject_local_model).await?;
    log::info!("OpenClaw gateway spawned with PID {}", pid);

    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        match wait_for_openclaw_status(60, |status| status.process_running && status.port_open)
            .await
        {
            Ok(final_status) => {
                let ready_url = final_status
                    .gateway_url
                    .clone()
                    .unwrap_or_else(|| gateway_url("127.0.0.1", OPENCLAW_GATEWAY_PORT));
                app_handle
                    .emit(
                        GATEWAY_READY_EVENT,
                        serde_json::json!({ "gateway_url": &ready_url }),
                    )
                    .ok();
                log::info!("OpenClaw gateway ready at {}", ready_url);
            }
            Err(e) => {
                log::warn!("OpenClaw gateway failed to become ready: {}", e);
            }
        }
    });

    Ok(OpenClawLaunchResult {
        gateway_url: gateway_url("127.0.0.1", OPENCLAW_GATEWAY_PORT),
    })
}

#[derive(serde::Serialize)]
pub struct OpenClawLaunchResult {
    pub gateway_url: String,
}

async fn kill_gateway_port_listener() {
    #[cfg(target_os = "windows")]
    {
        // Use PowerShell for reliable port-to-PID resolution and kill.
        let ps_script = format!(
            "$conn = netstat -ano | Select-String ':{port}'; foreach ($line in $conn) {{ $parts = $line -split '\\s+' | Where-Object {{ $_ -ne '' }}; if ($parts[-2] -eq 'LISTENING') {{ $procId = $parts[-1]; Stop-Process -Id $procId -Force -ErrorAction SilentlyContinue }} }}",
            port = OPENCLAW_GATEWAY_PORT
        );
        let _ = Command::new("powershell")
            .args(["-Command", &ps_script])
            .output()
            .await;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("pkill")
            .args(["-f", "openclaw gateway"])
            .output()
            .await;
    }
}

async fn kill_gateway_by_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/F"]);
        apply_no_window(&mut command);
        let _ = command.output().await;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .await;
    }
}

#[tauri::command]
pub async fn stop_openclaw_gateway() -> Result<(), String> {
    log::info!("Stopping OpenClaw gateway...");

    if let Some(pid) = take_gateway_pid() {
        log::info!("Killing OpenClaw gateway process (PID {})", pid);
        kill_gateway_by_pid(pid).await;
    }

    kill_gateway_port_listener().await;

    tokio::spawn(async move {
        match wait_for_openclaw_status(40, |status| {
            !status.process_running && !status.port_open
        })
        .await
        {
            Ok(_) => log::info!("OpenClaw gateway stopped successfully"),
            Err(e) => log::warn!("OpenClaw gateway stop verification timeout: {}", e),
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        filtered_openclaw_env, resolve_openclaw_install_registry, TEMPORARY_CHINA_NPM_REGISTRY,
    };

    #[test]
    fn filtered_openclaw_env_preserves_remote_provider_keys_without_local_injection() {
        let env = vec![
            ("OPENAI_API_KEY".to_string(), "openai-key".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "anthropic-key".to_string()),
            ("PATH".to_string(), "C:\\bin".to_string()),
        ];

        assert_eq!(filtered_openclaw_env(env.clone(), false), env);
    }

    #[test]
    fn filtered_openclaw_env_removes_remote_provider_keys_for_local_injection() {
        let filtered = filtered_openclaw_env(
            vec![
                ("OPENAI_API_KEY".to_string(), "openai-key".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "anthropic-key".to_string()),
                ("PATH".to_string(), "C:\\bin".to_string()),
            ],
            true,
        );

        assert_eq!(filtered, vec![("PATH".to_string(), "C:\\bin".to_string())]);
    }

    #[test]
    fn resolve_openclaw_install_registry_uses_temporary_mirror_for_default_npm_registry() {
        let decision = resolve_openclaw_install_registry(Some("https://registry.npmjs.org/"));

        assert_eq!(decision.as_deref(), Some(TEMPORARY_CHINA_NPM_REGISTRY));
    }

    #[test]
    fn resolve_openclaw_install_registry_keeps_existing_domestic_registry() {
        assert_eq!(
            resolve_openclaw_install_registry(Some("https://registry.npmmirror.com/")),
            None
        );
        assert_eq!(
            resolve_openclaw_install_registry(Some("https://registry.npm.taobao.org")),
            None
        );
    }

    #[test]
    fn resolve_openclaw_install_registry_keeps_custom_non_default_registry() {
        let decision = resolve_openclaw_install_registry(Some("https://packages.example.com/npm"));

        assert_eq!(decision, None);
    }

    #[test]
    fn resolve_openclaw_install_registry_uses_temporary_mirror_when_registry_is_empty() {
        let decision = resolve_openclaw_install_registry(Some("   "));

        assert_eq!(decision.as_deref(), Some(TEMPORARY_CHINA_NPM_REGISTRY));
    }

    #[test]
    fn resolve_openclaw_install_registry_uses_temporary_mirror_when_registry_is_unavailable() {
        let decision = resolve_openclaw_install_registry(None);

        assert_eq!(decision.as_deref(), Some(TEMPORARY_CHINA_NPM_REGISTRY));
    }

    #[tokio::test]
    #[ignore = "Requires OpenClaw to be installed; run manually with: cargo test -- --ignored openclaw_lifecycle"]
    async fn openclaw_lifecycle_launch_probe_stop() {
        use super::*;

        // 1. Ensure OpenClaw is installed.
        let bin = find_openclaw_binary().expect("OpenClaw must be installed for this test");

        // 2. Clean up any existing gateway.
        stop_openclaw_gateway().await.ok();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let status = get_openclaw_status_inner().await.unwrap();
        assert!(
            !status.process_running && !status.port_open,
            "Gateway should be stopped before test: {:?}",
            status
        );

        // 3. Launch gateway.
        let spawn_pid = spawn_gateway_foreground(&bin, false)
            .await
            .expect("Failed to spawn gateway");
        assert!(spawn_pid > 0);

        // 4. Wait for the gateway to become ready.
        let running_status = wait_for_openclaw_status(60, |s| {
            s.process_running && s.port_open
        })
        .await
        .expect("Gateway did not become ready");
        assert_eq!(running_status.health, "running");
        assert!(running_status.pid.is_some());
        assert!(running_status.gateway_url.is_some());

        // 5. Stop the gateway.
        stop_openclaw_gateway()
            .await
            .expect("Failed to stop gateway");

        // 6. Wait for the gateway to fully stop.
        let stopped_status = wait_for_openclaw_status(40, |s| {
            !s.process_running && !s.port_open
        })
        .await
        .expect("Gateway did not stop");
        assert_eq!(stopped_status.health, "stopped");
    }
}
