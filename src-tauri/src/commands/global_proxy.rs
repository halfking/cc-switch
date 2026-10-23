//! 全局出站代理相关命令
//!
//! 提供获取、设置和测试全局代理的 Tauri 命令。

use crate::proxy::http_client;
use crate::store::AppState;
use serde::Serialize;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

/// 获取全局代理 URL
///
/// 返回当前配置的代理 URL，null 表示直连。
#[tauri::command]
pub fn get_global_proxy_url(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let result = state.db.get_global_proxy_url().map_err(|e| e.to_string())?;
    log::debug!(
        "[GlobalProxy] [GP-010] Read from database: {}",
        result
            .as_ref()
            .map(|u| http_client::mask_url(u))
            .unwrap_or_else(|| "None".to_string())
    );
    Ok(result)
}

/// 设置全局代理 URL
///
/// - 传入非空字符串：启用代理
/// - 传入空字符串：清除代理（直连）
///
/// 执行顺序：先验证 → 写 DB → 再应用
/// 这样确保 DB 写失败时不会出现运行态与持久化不一致的问题
#[tauri::command]
pub fn set_global_proxy_url(state: tauri::State<'_, AppState>, url: String) -> Result<(), String> {
    // 调试：显示接收到的 URL 信息（不包含敏感内容）
    let has_auth = url.contains('@') && (url.starts_with("http://") || url.starts_with("socks"));
    log::debug!(
        "[GlobalProxy] [GP-011] Received URL: length={}, has_auth={}",
        url.len(),
        has_auth
    );

    let url_opt = if url.trim().is_empty() {
        None
    } else {
        Some(url.as_str())
    };

    // 1. 先验证代理配置是否有效（不应用）
    http_client::validate_proxy(url_opt)?;

    // 2. 验证成功后保存到数据库
    state
        .db
        .set_global_proxy_url(url_opt)
        .map_err(|e| e.to_string())?;

    // 3. DB 写入成功后再应用到运行态
    http_client::apply_proxy(url_opt)?;

    log::info!(
        "[GlobalProxy] [GP-009] Configuration updated: {}",
        url_opt
            .map(http_client::mask_url)
            .unwrap_or_else(|| "direct connection".to_string())
    );

    Ok(())
}

/// 代理测试结果
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResult {
    /// 是否连接成功
    pub success: bool,
    /// 延迟（毫秒）
    pub latency_ms: u64,
    /// 错误信息
    pub error: Option<String>,
}

/// 测试代理可达性所用的目标
///
/// 判定口径是「能收到任何 HTTP 响应就算通」——4xx/5xx 同样证明
/// TCP + TLS + 代理整条链路工作正常。
///
/// 历史实现用的是 `httpbin.org` / `google.com` / `api.anthropic.com` 的 HEAD。
/// 这三个目标在国内几乎必然超时，于是"测试通过/失败"与真实上游
/// （如 `api.openai.com`）完全对不上：代理明明不通，测试结果却不可信。
const PROXY_TEST_TARGETS: &[&str] = &[
    "https://api.openai.com/v1/models",
    "https://www.cloudflare.com/cdn-cgi/trace",
];

/// 测试代理连接
///
/// 通过指定的代理 URL 发送测试请求，返回连接结果和延迟。
/// 任一目标能拿到 HTTP 响应即认为代理可用。
#[tauri::command]
pub async fn test_proxy_url(url: String) -> Result<ProxyTestResult, String> {
    if url.trim().is_empty() {
        return Err("Proxy URL is empty".to_string());
    }

    let start = Instant::now();

    // 构建带代理的临时客户端
    let proxy = reqwest::Proxy::all(&url).map_err(|e| format!("Invalid proxy URL: {e}"))?;

    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Failed to build client: {e}"))?;

    let mut failures: Vec<String> = Vec::new();

    for test_url in PROXY_TEST_TARGETS {
        // 用 GET 而不是 HEAD：HEAD 在不少 CDN 上行为不一致（405/501 属于代理没坏）
        match client.get(*test_url).send().await {
            Ok(resp) => {
                let latency = start.elapsed().as_millis() as u64;
                log::debug!(
                    "[GlobalProxy] Test successful: {} -> {} via {} ({}ms)",
                    http_client::mask_url(&url),
                    test_url,
                    resp.status(),
                    latency
                );
                return Ok(ProxyTestResult {
                    success: true,
                    latency_ms: latency,
                    error: None,
                });
            }
            Err(e) => {
                // 保留真实传输层原因，否则用户只会看到 "error sending request"
                let reason = http_client::describe_transport_error(&e);
                log::debug!("[GlobalProxy] Test to {test_url} failed: {reason}");
                failures.push(format!("{test_url} -> {reason}"));
            }
        }
    }

    // 所有测试目标都失败：把每个目标的原因都带回去，区分
    // 「代理端口没人监听」与「代理能连但出网被封」
    let latency = start.elapsed().as_millis() as u64;
    let error_msg = if failures.is_empty() {
        "All test targets failed".to_string()
    } else {
        failures.join("; ")
    };

    log::debug!(
        "[GlobalProxy] Test failed: {} -> {} ({}ms)",
        http_client::mask_url(&url),
        error_msg,
        latency
    );

    Ok(ProxyTestResult {
        success: false,
        latency_ms: latency,
        error: Some(error_msg),
    })
}

/// 获取当前出站代理状态
///
/// 返回当前是否启用了出站代理以及代理 URL。
#[tauri::command]
pub fn get_upstream_proxy_status() -> UpstreamProxyStatus {
    let url = http_client::get_current_proxy_url();
    UpstreamProxyStatus {
        enabled: url.is_some(),
        proxy_url: url,
    }
}

/// 出站代理状态信息
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamProxyStatus {
    /// 是否启用代理
    pub enabled: bool,
    /// 代理 URL
    pub proxy_url: Option<String>,
}

/// 检测到的代理信息
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedProxy {
    /// 代理 URL
    pub url: String,
    /// 代理类型 (http/socks5)
    pub proxy_type: String,
    /// 端口
    pub port: u16,
}

/// 常见代理端口配置
/// 格式：(端口, 主要类型, 是否同时支持 http 和 socks5)
/// 对于 mixed 端口，会同时返回两种协议供用户选择
const PROXY_PORTS: &[(u16, &str, bool)] = &[
    (7890, "http", true),     // Clash (mixed mode)
    (7891, "socks5", false),  // Clash SOCKS only
    (1080, "socks5", false),  // 通用 SOCKS5
    (8080, "http", false),    // 通用 HTTP
    (8888, "http", false),    // Charles/Fiddler
    (3128, "http", false),    // Squid
    (10808, "socks5", false), // V2Ray SOCKS
    (10809, "http", false),   // V2Ray HTTP
];

/// 扫描本地代理
///
/// 检测常见端口是否有代理服务在运行。
/// 使用异步任务避免阻塞 UI 线程。
#[tauri::command]
pub async fn scan_local_proxies() -> Vec<DetectedProxy> {
    // 使用 spawn_blocking 避免阻塞主线程
    tokio::task::spawn_blocking(|| {
        let mut found = Vec::new();

        for &(port, primary_type, is_mixed) in PROXY_PORTS {
            let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
            if TcpStream::connect_timeout(&addr.into(), Duration::from_millis(100)).is_ok() {
                // 添加主要类型
                found.push(DetectedProxy {
                    url: format!("{primary_type}://127.0.0.1:{port}"),
                    proxy_type: primary_type.to_string(),
                    port,
                });
                // 对于 mixed 端口，同时添加另一种协议
                if is_mixed {
                    let alt_type = if primary_type == "http" {
                        "socks5"
                    } else {
                        "http"
                    };
                    found.push(DetectedProxy {
                        url: format!("{alt_type}://127.0.0.1:{port}"),
                        proxy_type: alt_type.to_string(),
                        port,
                    });
                }
            }
        }

        found
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取一个当场可证明"没人监听"的 loopback 代理地址
    ///
    /// 不能靠"释放一个本机端口"来制造死端口：临时端口区间（49152–65535）
    /// 同时是本机出站连接的源端口，本机 socket churn 密集，刚释放的端口
    /// 随时被占掉，`connect()` 会意外成功。特权端口 1/7/9 需要 root 才能
    /// bind，普通测试进程永远抢不走，再当场复核一次确实被拒绝。
    fn dead_proxy() -> String {
        use std::net::ToSocketAddrs;
        for port in [1u16, 7, 9] {
            let url = format!("http://127.0.0.1:{port}");
            if std::net::TcpStream::connect_timeout(
                &format!("127.0.0.1:{port}")
                    .to_socket_addrs()
                    .expect("loopback 必须能解析")
                    .next()
                    .expect("loopback 必须有地址"),
                std::time::Duration::from_millis(600),
            )
            .is_err()
            {
                return url;
            }
        }
        panic!("特权端口 1/7/9 都没被拒绝：本机有服务在监听，夹具前提不成立");
    }

    #[test]
    fn proxy_test_targets_exclude_domains_unreachable_in_cn() {
        // 历史实现用 httpbin.org / google.com：国内几乎必然超时，
        // 于是「测试通过」与真实上游（api.openai.com）完全对不上。
        assert!(!PROXY_TEST_TARGETS.is_empty(), "至少要有一个测试目标");
        for banned in ["httpbin.org", "google.com"] {
            assert!(
                !PROXY_TEST_TARGETS
                    .iter()
                    .any(|t| t.contains(banned)),
                "测试目标里不该再有国内不通的 {banned}: {PROXY_TEST_TARGETS:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_proxy_url_reports_actionable_cause_when_proxy_is_dead() {
        let dead = dead_proxy();
        let result = test_proxy_url(dead.clone())
            .await
            .expect("命令应返回 success=false，而不是 Err");

        assert!(!result.success, "代理没人监听时绝不能报成功");

        let err = result.error.expect("失败必须带原因");
        let lower = err.to_lowercase();
        assert!(
            lower.contains("refused") || lower.contains("connect"),
            "失败原因必须可行动（能看出是连不上代理），实际: {err}"
        );
    }
}
