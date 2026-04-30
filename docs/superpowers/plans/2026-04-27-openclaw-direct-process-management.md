# OpenClaw 直接进程管理实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 彻底移除对 OpenClaw CLI service/daemon 命令的依赖，将 Gateway 作为普通 Node.js 进程由 Rust 代码直接 spawn 和管理。

**Architecture:** 后端不再调用 `openclaw gateway status/start/stop/restart`，改为保存 spawn 的 PID、通过 TCP 端口探测检测状态、直接 kill 进程停止。前端状态机大幅简化，移除 service/runtime/port 等复杂状态。

**Tech Stack:** Rust (tokio, tauri), TypeScript (React)

---

## 文件结构

| 文件 | 职责 |
|------|------|
| `src-tauri/src/core/openclaw_launcher/commands.rs` | 核心重构：进程 spawn、PID 管理、TCP 探测、启动/停止/状态命令 |
| `web-app/src/hooks/useOpenClaw.ts` | 简化状态接口和数据结构 |
| `web-app/src/components/hub/OpenClawCard.tsx` | 简化状态展示（移除 service/rpc/config 详情行） |
| `web-app/src/routes/openclaw/index.tsx` | 简化传递给 Card 的 props |

---

## Task 1: Rust — 简化 OpenClawStatus 并添加进程管理基础设施

**Files:**
- Modify: `src-tauri/src/core/openclaw_launcher/commands.rs`

- [ ] **Step 1: 添加 PID 静态变量和 TCP 探测函数**

在 `commands.rs` 顶部（`use` 语句之后，`err_to_string` 之前）添加：

```rust
use std::sync::Mutex as StdMutex;

static GATEWAY_PID: StdMutex<Option<u32>> = StdMutex::new(None);

async fn is_port_open(host: &str, port: u16) -> bool {
    match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => true,
        _ => false,
    }
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Unix: kill -0 checks if process exists without sending a real signal
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
```

- [ ] **Step 2: 简化 `OpenClawStatus` 结构体**

将 `OpenClawStatus` 的定义（第 222-241 行）替换为：

```rust
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
```

- [ ] **Step 3: 重写 `openclaw_not_installed_status`**

将 `openclaw_not_installed_status` 函数（第 243-263 行）替换为：

```rust
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
```

- [ ] **Step 4: 编译检查**

Run: `cd src-tauri && cargo check`
Expected: 编译通过（可能有未使用函数 warning，后续任务清理）

---

## Task 2: Rust — 重写状态检测逻辑

**Files:**
- Modify: `src-tauri/src/core/openclaw_launcher/commands.rs`

- [ ] **Step 1: 重写 `get_openclaw_status_inner`**

将 `get_openclaw_status_inner` 函数（第 367-387 行）替换为：

```rust
async fn get_openclaw_status_inner() -> Result<OpenClawStatus, String> {
    let Some(bin) = find_openclaw_binary() else {
        return Ok(openclaw_not_installed_status());
    };

    let version = tokio::time::timeout(Duration::from_secs(5), openclaw_version(&bin))
        .await
        .unwrap_or(None);

    let pid = current_gateway_pid();
    let process_running = pid.map(is_process_alive).unwrap_or(false);
    let port_open = is_port_open("127.0.0.1", OPENCLAW_GATEWAY_PORT).await;

    let health = if process_running && port_open {
        "running"
    } else if process_running && !port_open {
        "degraded"
    } else if !process_running && port_open {
        // Port occupied by something else
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
```

- [ ] **Step 2: 重写 `wait_for_openclaw_status`**

将 `wait_for_openclaw_status` 函数（第 389-414 行）替换为：

```rust
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
```

- [ ] **Step 3: 编译检查**

Run: `cd src-tauri && cargo check`
Expected: 编译通过（parse_gateway_status 等旧函数 still 存在但可能 unused，不影响编译）

---

## Task 3: Rust — 重写启动和停止逻辑

**Files:**
- Modify: `src-tauri/src/core/openclaw_launcher/commands.rs`

- [ ] **Step 1: 重写 `spawn_gateway_foreground` 以保存 PID**

将 `spawn_gateway_foreground` 函数（第 638-672 行）替换为：

```rust
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

    let pid = child.id().ok_or("Failed to get child PID")?;
    set_gateway_pid(pid);

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

    // The child handle is dropped here but the process continues running
    // because stdout/stderr have been taken and piped to background tasks.
    Ok(pid)
}
```

- [ ] **Step 2: 重写 `launch_openclaw_gateway`**

将 `launch_openclaw_gateway` 函数（第 674-762 行）替换为：

```rust
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
    // Give the OS a moment to release the port.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let pid = spawn_gateway_foreground(&bin, inject_local_model).await?;
    log::info!("OpenClaw gateway spawned with PID {}", pid);

    // Wait for gateway readiness in background — don't block the command.
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
```

- [ ] **Step 3: 重写 `stop_openclaw_gateway`**

将 `stop_openclaw_gateway` 函数（第 793-840 行）替换为：

```rust
async fn kill_gateway_by_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .await;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output().await;
    }
}

#[tauri::command]
pub async fn stop_openclaw_gateway() -> Result<(), String> {
    log::info!("Stopping OpenClaw gateway...");

    // Kill by saved PID first.
    if let Some(pid) = take_gateway_pid() {
        log::info!("Killing OpenClaw gateway process (PID {})", pid);
        kill_gateway_by_pid(pid).await;
    }

    // Always do a port-based cleanup as a fallback.
    kill_gateway_port_listener().await;

    // Verify stop asynchronously — don't block the command.
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
```

- [ ] **Step 4: 编译检查**

Run: `cd src-tauri && cargo check`
Expected: 编译通过

---

## Task 4: Rust — 清理未使用代码并更新测试

**Files:**
- Modify: `src-tauri/src/core/openclaw_launcher/commands.rs`

- [ ] **Step 1: 移除未使用的函数**

删除以下函数（它们不再被调用）：
- `json_at_path`（第 115-121 行）
- `json_bool`（第 123-125 行）
- `json_string`（第 127-129 行）
- `json_u16`（第 131-135 行）
- `is_runtime_running`（第 137-142 行）
- `is_runtime_error`（第 144-149 行）
- `is_port_busy`（第 151-153 行）
- `compute_health`（第 163-173 行）
- `build_status_message`（第 175-215 行）
- `parse_gateway_status`（第 265-324 行）
- `command_error`（第 359-365 行）
- `run_openclaw_command`（第 338-357 行）——**注意：保留 `openclaw_version` 因为它仍被使用**

- [ ] **Step 2: 更新测试模块**

将测试模块（第 842-996 行）替换为：

```rust
#[cfg(test)]
mod tests {
    use super::{filtered_openclaw_env, resolve_openclaw_install_registry, TEMPORARY_CHINA_NPM_REGISTRY};

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
}
```

- [ ] **Step 3: 编译和测试**

Run: `cd src-tauri && cargo test -- openclaw`
Expected: 所有测试通过

Run: `cd src-tauri && cargo check`
Expected: 编译通过，无 unused 警告

---

## Task 5: 前端 — 简化 useOpenClaw hook

**Files:**
- Modify: `web-app/src/hooks/useOpenClaw.ts`

- [ ] **Step 1: 简化类型定义**

将 `OpenClawDiagnostics` 接口（第 31-43 行）替换为：

```typescript
export interface OpenClawDiagnostics {
  processRunning: boolean
  pid?: number
  portOpen: boolean
  health: string
}
```

将 `OpenClawBackendStatus` 接口（第 55-73 行）替换为：

```typescript
interface OpenClawBackendStatus {
  installed: boolean
  binary_path?: string
  version?: string
  gateway_url?: string
  gateway_port?: number
  process_running: boolean
  pid?: number
  port_open: boolean
  health: string
  message?: string
}
```

- [ ] **Step 2: 重写 `toUiStatus`**

将 `toUiStatus` 函数（第 94-100 行）替换为：

```typescript
function toUiStatus(status: OpenClawBackendStatus): OpenClawStatus {
  if (!status.installed) return 'not-installed'
  if (status.health === 'running') return 'running'
  if (status.health === 'degraded') return 'degraded'
  if (status.health === 'error') return 'error'
  return 'installed'
}
```

- [ ] **Step 3: 重写 `toDiagnostics`**

将 `toDiagnostics` 函数（第 102-116 行）替换为：

```typescript
function toDiagnostics(status: OpenClawBackendStatus): OpenClawDiagnostics {
  return {
    processRunning: status.process_running,
    pid: status.pid,
    portOpen: status.port_open,
    health: status.health,
  }
}
```

- [ ] **Step 4: 更新 `useOpenClaw` 内部状态初始化**

将 `diagnostics` 的初始化（第 131-143 行）替换为：

```typescript
  const [diagnostics, setDiagnostics] = useState<OpenClawDiagnostics>({
    processRunning: false,
    pid: undefined,
    portOpen: false,
    health: 'not-installed',
  })
```

- [ ] **Step 5: TypeScript 检查**

Run: `cd web-app && yarn tsc --noEmit`
Expected: 编译通过（OpenClawCard 中引用的旧字段可能报错，下一任务修复）

---

## Task 6: 前端 — 简化 OpenClawCard 组件

**Files:**
- Modify: `web-app/src/components/hub/OpenClawCard.tsx`

- [ ] **Step 1: 简化 Props 接口**

将 `OpenClawCardProps` 接口（第 71-89 行）替换为：

```typescript
export interface OpenClawCardProps {
  status: OpenClawStatus
  version?: string
  gatewayUrl?: string
  installProgress?: number
  installMessage?: string
  processRunning?: boolean
  pid?: number
  portOpen?: boolean
  onInstall?: () => void
  onStart?: () => void
  onStop?: () => void
  onRestart?: () => void
  onOpenDashboard?: () => void
  onConfigure?: () => void
  onRefresh?: () => void | Promise<void>
  isLoading?: boolean
  className?: string
}
```

- [ ] **Step 2: 更新组件函数签名和解构**

将组件函数参数解构（第 91-109 行）替换为：

```typescript
export function OpenClawCard({
  status,
  version,
  gatewayUrl,
  installProgress = 0,
  installMessage = '',
  processRunning,
  pid,
  portOpen,
  onInstall,
  onStart,
  onStop,
  onRestart,
  onOpenDashboard,
  onConfigure,
  onRefresh,
  isLoading = false,
  className,
}: OpenClawCardProps) {
```

- [ ] **Step 3: 简化 summaryItems**

将 `summaryItems` 数组（第 123-129 行）替换为：

```typescript
  const summaryItems = [
    version ? { label: '版本', value: version } : null,
    gatewayUrl ? { label: 'Gateway', value: gatewayUrl, mono: true } : null,
    pid ? { label: 'PID', value: String(pid) } : null,
    processRunning !== undefined ? { label: '进程', value: processRunning ? '运行中' : '未运行' } : null,
    portOpen !== undefined ? { label: '端口', value: portOpen ? '已监听' : '未监听' } : null,
  ].filter(Boolean) as { label: string; value: string; mono?: boolean }[]
```

- [ ] **Step 4: TypeScript 检查**

Run: `cd web-app && yarn tsc --noEmit`
Expected: 可能 still 有 `openclaw/index.tsx` 的报错，下一任务修复

---

## Task 7: 前端 — 简化 OpenClaw 页面

**Files:**
- Modify: `web-app/src/routes/openclaw/index.tsx`

- [ ] **Step 1: 简化传递给 Card 的 props**

将 `OpenClawContent` 函数中相关变量（第 46-59 行）替换为：

```typescript
  const isTransitioning = openClawStatus === 'starting' || openClawStatus === 'stopping'
```

将 `OpenClawCard` 组件调用（第 118-135 行）中的 props 替换为：

```tsx
            <OpenClawCard
              status={openClawStatus}
              version={openClawVersion}
              gatewayUrl={openClawGatewayUrl}
              installProgress={openClawInstallProgress}
              installMessage={openClawErrorMessage ?? openClawInstallMessage}
              processRunning={diagnostics.processRunning}
              pid={diagnostics.pid}
              portOpen={diagnostics.portOpen}
              onInstall={installOpenClaw}
              onStart={handleStartOpenClaw}
              onStop={handleStopOpenClaw}
              onRestart={handleRestartOpenClaw}
              onOpenDashboard={openOpenClawDashboard}
              onConfigure={handleManageOpenClaw}
              onRefresh={refreshOpenClaw}
              isLoading={isOpenClawLoading}
            />
```

- [ ] **Step 2: TypeScript 检查**

Run: `cd web-app && yarn tsc --noEmit`
Expected: 编译通过，无错误

---

## Task 8: 最终验证

- [ ] **Step 1: Rust 编译和测试**

Run: `cd src-tauri && cargo test`
Expected: 所有测试通过

Run: `cd src-tauri && cargo check`
Expected: 编译通过，无 warning

- [ ] **Step 2: TypeScript 编译**

Run: `cd web-app && yarn tsc --noEmit`
Expected: 编译通过，无错误

- [ ] **Step 3: 提交**

```bash
git add src-tauri/src/core/openclaw_launcher/commands.rs
 git add web-app/src/hooks/useOpenClaw.ts
 git add web-app/src/components/hub/OpenClawCard.tsx
 git add web-app/src/routes/openclaw/index.tsx
 git commit -m "refactor: take full control of OpenClaw gateway lifecycle

Remove dependency on OpenClaw CLI service/daemon commands:
- Replace openclaw gateway status/start/stop/restart with direct process management
- Track spawned gateway PID in static variable
- Detect health via TCP port probe + process liveness check
- Simplify backend status struct (remove service_* fields)
- Simplify frontend state machine and diagnostics"
```

- [ ] **Step 4: 推送**

```bash
git push origin main
```

---

## Self-Review Checklist

**1. Spec coverage:**
- ✅ 不再调用 `openclaw gateway start/restart` — Task 3 Step 2
- ✅ 不再调用 `openclaw gateway stop` — Task 3 Step 3
- ✅ 不再调用 `openclaw gateway status` — Task 2 Step 1
- ✅ 统一走 `openclaw gateway run --force` 前台启动 — Task 3 Step 2
- ✅ 保存 PID 并直接 kill — Task 1 Step 1, Task 3 Step 3
- ✅ TCP 端口探测替代 status 命令 — Task 1 Step 1, Task 2 Step 1
- ✅ 前端状态机简化 — Task 5, Task 6, Task 7
- ✅ 编译和测试通过 — Task 4, Task 8

**2. Placeholder scan:**
- ✅ 无 TBD/TODO
- ✅ 所有步骤包含具体代码
- ✅ 所有步骤包含具体命令和预期输出

**3. Type consistency:**
- ✅ Rust `OpenClawStatus` 字段与前端 `OpenClawBackendStatus` 对应（`process_running` ↔ `processRunning` 等）
- ✅ `health` 字符串值前后端一致：`not-installed`, `stopped`, `running`, `degraded`, `error`
