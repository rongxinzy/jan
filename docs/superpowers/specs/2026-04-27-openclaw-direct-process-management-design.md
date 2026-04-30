# OpenClaw 直接进程管理设计文档

> 目标：彻底移除对 OpenClaw CLI service/daemon 命令的依赖，将 Gateway 作为普通 Node.js 进程由 Rust 代码直接 spawn 和管理。

---

## 背景与问题

当前 `launch_openclaw_gateway` 混合使用了两种启动模式：

1. **Service 模式**：当 `service_loaded=true` 时，调用 `openclaw gateway start/restart`（通过 Windows Task Scheduler / launchd / systemd 管理）
2. **前台模式**：当 `service_loaded=false` 时，调用 `openclaw gateway run --force`（前台 spawn）

这种混合导致：
- 状态机极其复杂（`service_loaded` × `rpc_ok` × `runtime_status` × `port_status` 的组合爆炸）
- 前端需要理解 OpenClaw 的 5+ 种内部状态
- Service 管理引入不可控因素（Task Scheduler 延迟、权限问题、残留状态）
- Bug 难以复现和排查

## 核心设计原则

**把 OpenClaw Gateway 当作一个普通的 Node.js 进程来管理。**

- ❌ 不再调用 `openclaw gateway start/stop/restart/status`
- ✅ 统一使用 `openclaw gateway run --force` 前台启动
- ✅ 进程 PID 由 Rust 代码保存和管理
- ✅ 状态检测改为 HTTP health probe
- ✅ 停止时直接 kill 进程

## 架构变更

### 启动流程（简化后）

```
launch_openclaw_gateway(model):
  1. 确保 ~/.openclaw/openclaw.json 已写入（Ollama provider 注入）
  2. 清理残留：kill 端口 18789 上的所有进程
  3. 设置环境变量（剥离远程 provider API keys）
  4. 直接 spawn: openclaw gateway run --force --allow-unconfigured
  5. 保存 Child PID 到内存状态
  6. 立即返回（声明式）
  7. 后台轮询 HTTP health endpoint 确认就绪
```

### 停止流程（简化后）

```
stop_openclaw_gateway():
  1. 如果有保存的 Child PID → 先向 PID 发终止信号
  2. kill 端口 18789 上的所有进程（兜底）
  3. 清除保存的 PID
  4. 立即返回（声明式）
  5. 后台轮询确认端口释放
```

### 状态检测（去 CLI 化）

| 行为 | 旧方式 | 新方式 |
|------|--------|--------|
| 判断是否运行 | `openclaw gateway status --json` | HTTP GET `http://127.0.0.1:18789/health` |
| 判断是否就绪 | 解析 JSON 中的 `rpc.ok` | HTTP 200 + 响应体健康标志 |
| 获取版本 | `openclaw --version` | 保留（一次性查询，不阻塞生命周期） |

### 前端状态机简化

| 旧状态 | 新状态 | 说明 |
|--------|--------|------|
| `not_installed` | `not_installed` | 未安装，不变 |
| `installed` | `stopped` | 已安装但未运行 |
| `starting` | `starting` | 启动中（命令已发送，等待 health probe） |
| `running` | `running` | Gateway 运行中（health probe 通过） |
| `degraded` | `degraded` | Gateway 进程存在但 health probe 异常 |
| `stopping` | `stopping` | 停止中 |
| `error` | `error` | 错误状态 |

移除以下概念：
- `service_loaded` — 不再关心 OpenClaw 内部 service 注册状态
- `service_runtime_status` — 不再关心 Task Scheduler / systemd 状态
- `rpc_ok` — 由 HTTP health probe 替代
- `port_status` — 由进程存在性 + health probe 替代

## 文件改动清单

### Rust 后端

| 文件 | 改动类型 | 说明 |
|------|----------|------|
| `src-tauri/src/core/openclaw_launcher/commands.rs` | 大幅重构 | 移除 `run_openclaw_command` 的 CLI 子命令调用；简化 `launch`/`stop`/`status` |
| `src-tauri/src/core/openclaw_launcher/models.rs` | 修改 | 简化 `OpenClawBackendStatus` 结构体，移除 service 相关字段 |
| `src-tauri/src/core/openclaw_launcher/mod.rs` | 可能修改 | 如有需要，调整模块导出 |
| `src-tauri/src/lib.rs` | 不变 | command 注册不变 |
| `src-tauri/tauri.conf.json` | 检查 | 确保 `shell` 权限允许 spawn |

### TypeScript 前端

| 文件 | 改动类型 | 说明 |
|------|----------|------|
| `web-app/src/hooks/useOpenClaw.ts` | 大幅重构 | 简化状态机；`refresh` 改为 HTTP health probe |
| `web-app/src/routes/openclaw/index.tsx` | 修改 | 移除 service 状态显示；简化 UI |
| `web-app/src/components/hub/OpenClawCard.tsx` | 修改 | 简化状态展示 |
| `web-app/src/components/hub/OpenClawConfigSummary.tsx` | 不变 | 配置摘要不变 |
| `web-app/src/components/hub/OpenClawConfigDialog.tsx` | 不变 | 配置对话框不变 |

## 关键实现细节

### 1. 进程 Spawn 与 PID 管理

```rust
// 使用 tokio::process::Command 保存 Child 句柄
static GATEWAY_CHILD: Mutex<Option<tokio::process::Child>> = Mutex::const_new(None);

async fn spawn_gateway(bin: &str, inject_local_model: bool) -> Result<u32, String> {
    let mut command = Command::new(bin);
    command.args(["gateway", "run", "--force", "--allow-unconfigured"]);
    command.envs(openclaw_env(inject_local_model));
    apply_no_window(&mut command);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    
    let mut child = command.spawn()
        .map_err(|e| format!("Failed to spawn OpenClaw gateway: {e}"))?;
    
    let pid = child.id().ok_or("Failed to get child PID")?;
    
    // 保存 Child 句柄
    let mut guard = GATEWAY_CHILD.lock().await;
    *guard = Some(child);
    
    Ok(pid)
}
```

### 2. HTTP Health Probe

```rust
async fn probe_gateway_health(port: u16) -> Result<bool, reqwest::Error> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    
    // OpenClaw Gateway 的健康端点可能是 /health 或类似路径
    // 需要实际探测确认
    match client.get(format!("http://127.0.0.1:{port}/health")).send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(e) if e.is_connect() => Ok(false), // 端口未监听 = 未运行
        Err(e) => Err(e),
    }
}
```

### 3. 进程 Kill（Windows）

```rust
async fn kill_gateway_by_port(port: u16) {
    // Windows: 先找到占用端口的 PID，再 taskkill
    let output = Command::new("cmd")
        .args(["/C", &format!(
            "for /f \"tokens=5\" %a in ('netstat -ano ^| findstr \":{port}\"') do taskkill /PID %a /F 2>nul"
        , port)])
        .output()
        .await;
    // ...
}
```

### 4. 环境变量

保留现有逻辑：
- `inject_local_model=true` 时剥离远程 provider API keys
- 其他环境变量透传

## 风险与缓解

| 风险 | 缓解措施 |
|------|----------|
| `--allow-unconfigured` 参数可能不存在于旧版 OpenClaw | 添加版本检查；或捕获 spawn 错误后回退到不带该参数 |
| `/health` 端点路径不确定 | 实现时先通过 `openclaw gateway status` 一次性探测确认；或尝试多个常见路径 |
| Gateway 启动后可能自行 fork daemon | 通过 `--force` 和 `--allow-unconfigured` 参数确保前台运行；monitor PID |
| 多实例冲突 | 启动前强制 kill 旧进程 + 端口清理 |

## 回滚策略

- 所有改动集中在 `openclaw_launcher` 模块
- 前端状态机虽有简化，但向后兼容（减少状态不会破坏旧代码）
- 如需回滚：git revert 单条 commit 即可恢复

## 测试策略

1. **单元测试**：健康 probe 的超时和重试逻辑
2. **集成测试**：启动 → health probe 通过 → 停止 → 端口释放 的完整流程
3. **手动测试**：
   - 多次快速启动/停止
   - 启动后强制 kill 外部进程，观察状态恢复
   - 注入本地模型后启动，验证配置生效

## 验收标准

- [ ] `launch_openclaw_gateway` 不再调用 `openclaw gateway start/restart`
- [ ] `stop_openclaw_gateway` 不再调用 `openclaw gateway stop`
- [ ] `get_openclaw_status` 不再调用 `openclaw gateway status`
- [ ] 启动/停止/状态检测全部通过 HTTP health probe 完成
- [ ] 前端状态机仅保留：`not_installed`, `stopped`, `starting`, `running`, `degraded`, `stopping`, `error`
- [ ] 编译通过（Rust + TypeScript）
- [ ] 手动验证：完整启动/停止流程正常
