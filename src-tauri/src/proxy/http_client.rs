//! 全局 HTTP 客户端模块
//!
//! 提供支持全局代理配置的 HTTP 客户端。
//! 所有需要发送 HTTP 请求的模块都应使用此模块提供的客户端。

use once_cell::sync::OnceCell;
use reqwest::Client;
use std::env;
use std::error::Error as _;
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::sync::RwLock;
use std::time::Duration;

/// 全局 HTTP 客户端实例
static GLOBAL_CLIENT: OnceCell<RwLock<Client>> = OnceCell::new();

/// 当前代理 URL（用于日志和状态查询）
static CURRENT_PROXY_URL: OnceCell<RwLock<Option<String>>> = OnceCell::new();

/// CC Switch 代理服务器当前监听的端口
static CC_SWITCH_PROXY_PORT: OnceCell<RwLock<u16>> = OnceCell::new();

/// 设置 CC Switch 代理服务器的监听端口
///
/// 应在代理服务器启动时调用，以便系统代理检测能正确识别自己的端口
pub fn set_proxy_port(port: u16) {
    if let Some(lock) = CC_SWITCH_PROXY_PORT.get() {
        if let Ok(mut current_port) = lock.write() {
            *current_port = port;
            log::debug!("[GlobalProxy] Updated CC Switch proxy port to {port}");
        }
    } else {
        let _ = CC_SWITCH_PROXY_PORT.set(RwLock::new(port));
        log::debug!("[GlobalProxy] Initialized CC Switch proxy port to {port}");
    }
}

/// 获取 CC Switch 代理服务器的监听端口
fn get_proxy_port() -> u16 {
    CC_SWITCH_PROXY_PORT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|port| *port)
        .unwrap_or(15721) // 默认端口作为回退
}

/// 初始化全局 HTTP 客户端
///
/// 应在应用启动时调用一次。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，如 `http://127.0.0.1:7890` 或 `socks5://127.0.0.1:1080`
///   传入 None 或空字符串表示直连
pub fn init(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let client = build_client(effective_url)?;

    // 尝试初始化全局客户端，如果已存在则记录警告并使用 apply_proxy 更新
    if GLOBAL_CLIENT.set(RwLock::new(client.clone())).is_err() {
        log::warn!(
            "[GlobalProxy] [GP-003] Already initialized, updating instead: {}",
            describe_effective_egress(effective_url)
        );
        // 已初始化，改用 apply_proxy 更新
        return apply_proxy(proxy_url);
    }

    // 初始化代理 URL 记录
    let _ = CURRENT_PROXY_URL.set(RwLock::new(effective_url.map(|s| s.to_string())));

    log::info!(
        "[GlobalProxy] Initialized: {}",
        describe_effective_egress(effective_url)
    );

    warn_if_proxy_unreachable(effective_url);

    Ok(())
}

/// 验证代理配置（不应用）
///
/// 只验证代理 URL 是否有效，不实际更新全局客户端。
/// 用于在持久化之前验证配置的有效性。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
///
/// # Returns
/// 验证成功返回 Ok(())，失败返回错误信息
pub fn validate_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    // 只调用 build_client 来验证，但不应用
    build_client(effective_url)?;
    Ok(())
}

/// 应用代理配置（假设已验证）
///
/// 直接应用代理配置到全局客户端，不做额外验证。
/// 应在 validate_proxy 成功后调用。
///
/// # Arguments
/// * `proxy_url` - 代理 URL，None 或空字符串表示直连
pub fn apply_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }

    log::info!(
        "[GlobalProxy] Applied: {}",
        describe_effective_egress(effective_url)
    );

    warn_if_proxy_unreachable(effective_url);

    Ok(())
}

/// 更新代理配置（热更新）
///
/// 可在运行时调用以更改代理设置，无需重启应用。
/// 注意：此函数同时验证和应用，如果需要先验证后持久化再应用，
/// 请使用 validate_proxy + apply_proxy 组合。
///
/// # Arguments
/// * `proxy_url` - 新的代理 URL，None 或空字符串表示直连
#[allow(dead_code)]
pub fn update_proxy(proxy_url: Option<&str>) -> Result<(), String> {
    let effective_url = proxy_url.filter(|s| !s.trim().is_empty());
    let new_client = build_client(effective_url)?;

    // 更新客户端
    if let Some(lock) = GLOBAL_CLIENT.get() {
        let mut client = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-001] Failed to acquire write lock: {e}");
            "Failed to update proxy: lock poisoned".to_string()
        })?;
        *client = new_client;
    } else {
        // 如果还没初始化，则初始化
        return init(proxy_url);
    }

    // 更新代理 URL 记录
    if let Some(lock) = CURRENT_PROXY_URL.get() {
        let mut url = lock.write().map_err(|e| {
            log::error!("[GlobalProxy] [GP-002] Failed to acquire URL write lock: {e}");
            "Failed to update proxy URL record: lock poisoned".to_string()
        })?;
        *url = effective_url.map(|s| s.to_string());
    }

    log::info!(
        "[GlobalProxy] Updated: {}",
        describe_effective_egress(effective_url)
    );

    warn_if_proxy_unreachable(effective_url);

    Ok(())
}

/// 获取全局 HTTP 客户端
///
/// 返回配置了代理的客户端（如果已配置代理），否则返回跟随系统代理的客户端。
pub fn get() -> Client {
    GLOBAL_CLIENT
        .get()
        .and_then(|lock| lock.read().ok())
        .map(|c| c.clone())
        .unwrap_or_else(|| {
            log::warn!("[GlobalProxy] [GP-004] Client not initialized, using fallback");
            build_client(None).unwrap_or_default()
        })
}

/// 获取当前代理 URL
///
/// 返回当前配置的代理 URL，None 表示直连。
pub fn get_current_proxy_url() -> Option<String> {
    CURRENT_PROXY_URL
        .get()
        .and_then(|lock| lock.read().ok())
        .and_then(|url| url.clone())
}

/// 检查是否正在使用代理
#[allow(dead_code)]
pub fn is_proxy_enabled() -> bool {
    get_current_proxy_url().is_some()
}

/// 构建 HTTP 客户端
fn build_client(proxy_url: Option<&str>) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        // 禁用 reqwest 自动解压：防止 reqwest 覆盖客户端原始 accept-encoding header。
        // 响应解压由 response_processor 根据 content-encoding 手动处理。
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();

    // 有代理地址则使用代理，否则跟随系统代理
    if let Some(url) = proxy_url {
        // 先验证 URL 格式和 scheme
        let parsed = url::Url::parse(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;

        let scheme = parsed.scheme();
        if !["http", "https", "socks5", "socks5h"].contains(&scheme) {
            return Err(format!(
                "Invalid proxy scheme '{}' in URL '{}'. Supported: http, https, socks5, socks5h",
                scheme,
                mask_url(url)
            ));
        }

        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| format!("Invalid proxy URL '{}': {}", mask_url(url), e))?;
        builder = builder.proxy(proxy);
        log::debug!("[GlobalProxy] Proxy configured: {}", mask_url(url));
    } else {
        // 未设置全局代理时，让 reqwest 自动检测系统代理（环境变量）
        // 若系统代理指向本机，禁用系统代理避免自环
        if system_proxy_points_to_loopback() {
            builder = builder.no_proxy();
            log::warn!(
                "[GlobalProxy] System proxy points to localhost, bypassing to avoid recursion"
            );
        } else {
            log::debug!("[GlobalProxy] Following system proxy (no explicit proxy configured)");
        }
    }

    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn system_proxy_points_to_loopback() -> bool {
    const KEYS: [&str; 6] = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];

    // 环境变量与 macOS 系统代理都要看：reqwest 读的是两者（环境变量优先，
    // 系统代理补空），只查环境变量的闸门在「系统代理指向自己」时形同虚设，
    // 而那正是这道闸要防的自环。
    KEYS.iter()
        .filter_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .chain(os_system_proxy())
        .any(|value| proxy_points_to_loopback(&value))
}

fn proxy_points_to_loopback(value: &str) -> bool {
    fn host_is_loopback(host: &str) -> bool {
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        host.parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }

    // 检查是否指向 CC Switch 自己的代理端口
    // 只有指向自己的代理才需要跳过，避免递归
    fn is_cc_switch_proxy_port(port: Option<u16>) -> bool {
        let cc_switch_port = get_proxy_port();
        port == Some(cc_switch_port)
    }

    if let Ok(parsed) = url::Url::parse(value) {
        if let Some(host) = parsed.host_str() {
            // 只有当主机是 loopback 且端口是 CC Switch 的端口时才返回 true
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
        return false;
    }

    let with_scheme = format!("http://{value}");
    if let Ok(parsed) = url::Url::parse(&with_scheme) {
        if let Some(host) = parsed.host_str() {
            return host_is_loopback(host) && is_cc_switch_proxy_port(parsed.port());
        }
    }

    false
}

/// 隐藏 URL 中的敏感信息（用于日志）
pub fn mask_url(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        // 隐藏用户名和密码，保留 scheme、host 和端口
        let host = parsed.host_str().unwrap_or("?");
        match parsed.port() {
            Some(port) => format!("{}://{}:{}", parsed.scheme(), host, port),
            None => format!("{}://{}", parsed.scheme(), host),
        }
    } else {
        // URL 解析失败，返回部分内容。截断点回退到最近的字符边界，
        // 避免在多字节 UTF-8 字符中间切割导致 panic。
        if url.len() > 20 {
            let cut = (0..=20)
                .rev()
                .find(|&i| url.is_char_boundary(i))
                .unwrap_or(0);
            format!("{}...", &url[..cut])
        } else {
            url.to_string()
        }
    }
}

/// 去掉错误文本里内嵌的 URL
///
/// 错误文本里的 URL 可能带签名 query / token，落进日志与 DB 就等于泄露凭据。
fn strip_urls(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        // 两种 scheme 长度不同（"http://" 7 字符 / "https://" 8 字符），
        // 取 8 字符比较，否则 https 的 URL 永远匹配不上、凭据会原样进日志。
        let head: String = chars[i..chars.len().min(i + 8)].iter().collect();
        if head == "http://" || head == "https://" {
            // 吞到空白或圆括号为止
            let mut j = i;
            while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '(' && chars[j] != ')'
            {
                j += 1;
            }
            out.push_str("<url>");
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 从 reqwest 错误里挖出真实的传输层原因
///
/// reqwest 的顶层 Display 只有一句 "error sending request"，真实原因
/// （Connection refused / dns 解析失败 / TLS 握手失败）藏在 source chain 里。
/// 历史实现只取 `without_url()`，于是每一个失败请求在日志和 DB 里长得一模一样，
/// 排查时分不清"代理端口没人监听"和"域名解析不了"——本模块引入前，
/// OpenAI official 全量 502 就是这么查不出来的。
pub fn describe_transport_error(error: &reqwest::Error) -> String {
    let mut parts: Vec<String> = Vec::new();

    let head = strip_urls(&error.to_string()).trim().to_string();
    if !head.is_empty() {
        parts.push(head);
    }

    let mut inner = error.source();
    let mut depth = 0usize;
    while let Some(err) = inner {
        if depth >= 8 {
            break;
        }
        let text = strip_urls(&err.to_string()).trim().to_string();
        // source chain 会重复携带上层信息，去重后只留新增的那一段
        if !text.is_empty() && !parts.iter().any(|p| p == &text) {
            parts.push(text);
        }
        inner = err.source();
        depth += 1;
    }

    if parts.is_empty() {
        "未知传输错误".to_string()
    } else {
        parts.join(" <- ")
    }
}

/// 解析 macOS `scutil --proxy` 的输出，取 https 目标的系统代理
///
/// 只取纯文本解析，抽出来是为了能单测：`scutil` 要真机才有，
/// 但「这段输出该解析成什么」是纯函数。
///
/// 口径与 hyper-util 的 `mac::with_system` 对齐：HTTPS 优先，
/// HTTPS 没开才回落 HTTP。`ProxyAutoConfigEnable`（PAC）无法还原成
/// 一个静态代理地址，此时返回 `None`——宁可少报，不可报错地址。
fn parse_scutil_proxy(output: &str) -> Option<String> {
    let field = |key: &str| -> Option<String> {
        output.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim() != key {
                return None;
            }
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
    };

    let enabled = |key: &str| field(key).as_deref() == Some("1");

    // PAC（自动配置脚本）不是静态代理地址，跳过而不是硬凑一个
    if enabled("ProxyAutoConfigEnable") && field("ProxyAutoConfigURLString").is_some() {
        return None;
    }

    let from = |enable_key: &str, host_key: &str, port_key: &str| -> Option<String> {
        if !enabled(enable_key) {
            return None;
        }
        let host = field(host_key)?;
        let port = field(port_key)?;
        Some(format!("http://{host}:{port}"))
    };

    from("HTTPSEnable", "HTTPSProxy", "HTTPSPort")
        .or_else(|| from("HTTPEnable", "HTTPProxy", "HTTPPort"))
}

/// 读取操作系统层面的系统代理（macOS）
///
/// 这不是可有可无的补充。reqwest 0.12 默认启用 `system-proxy` feature，
/// 未显式配置代理时会走 `ProxyMatcher::system()` →
/// hyper-util `Matcher::from_system()`，它在读完环境变量后
/// **还会去读 macOS SystemConfiguration**（`mac::with_system`）。
///
/// 历史实现只查环境变量，于是这台机器上「GUI 应用没有代理环境变量、
/// 系统代理指向 192.168.31.30:7897」的真实形态完全不可见：
/// 日志打印 "direct connection"，实际每个上游请求都在隧道里走系统代理。
#[cfg(target_os = "macos")]
fn os_system_proxy() -> Option<String> {
    let output = std::process::Command::new("scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_scutil_proxy(&String::from_utf8_lossy(&output.stdout))
}

/// 非 macOS 平台暂不解析系统代理，退回只看环境变量。
///
/// reqwest 在 Windows 上同样会读系统代理，这里是已知的覆盖缺口；
/// 与其塞一段没在本机验证过的解析逻辑，不如先如实标出来。
#[cfg(not(target_os = "macos"))]
fn os_system_proxy() -> Option<String> {
    None
}

/// 解析当前真正生效的出站代理地址
///
/// 取值顺序必须与 reqwest/hyper-util 的实际取值顺序一致，
/// 否则探测的是一个根本没在用的代理地址（告警就成了恒真判据）：
///
/// 1. cc-switch 显式配置（`ClientBuilder::proxy` 会关掉 auto_sys_proxy）
/// 2. 环境变量 `HTTPS_PROXY` / `HTTP_PROXY`
/// 3. macOS 系统代理（仅在环境变量为空时补位，见 `mac::with_system`）
///
/// `ALL_PROXY` 不在其中：hyper-util 的 `from_env()` 只读
/// `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`，把它算进来属于探测了一个
/// 根本不会被使用的地址。
fn effective_proxy_from(explicit: Option<&str>, os_proxy: Option<&str>) -> Option<String> {
    if let Some(u) = explicit.filter(|s| !s.trim().is_empty()) {
        return Some(u.trim().to_string());
    }
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
        if let Ok(value) = env::var(key) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    os_proxy
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn effective_proxy_for_probe(explicit: Option<&str>) -> Option<String> {
    effective_proxy_from(explicit, os_system_proxy().as_deref())
}

/// 供日志使用的出口描述
///
/// 历史实现没配显式代理时就打印 "direct connection"，而实际上 reqwest
/// 会去读 macOS 系统代理——这句话是假的。2026-10-14 那次 Codex 全量 502
/// 就是在这句日志下查不下去的：日志说直连，DB 里却是
/// "tunnel error ... Connection refused"，真相是每个请求都在隧道里
/// 走一个已经没人监听的系统代理。
fn describe_effective_egress(explicit: Option<&str>) -> String {
    if let Some(url) = explicit.filter(|s| !s.trim().is_empty()) {
        return mask_url(url);
    }
    match effective_proxy_for_probe(None) {
        Some(url) => format!("{}（来自系统代理，cc-switch 未显式配置）", mask_url(&url)),
        None => "直连（无显式配置，系统也未设置代理）".to_string(),
    }
}

/// 探测出站代理端点是否可达（只做 TCP 连接，不发应用层请求）
fn probe_proxy_endpoint(url: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(url).map_err(|_| format!("代理地址无法解析: {}", mask_url(url)))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| "代理地址缺少主机名".to_string())?;

    let port = parsed.port().unwrap_or_else(|| match parsed.scheme() {
        "socks5" | "socks5h" => 1080,
        "https" => 443,
        _ => 80,
    });

    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析失败: {}", e))?
        .next()
        .ok_or_else(|| "DNS 未返回任何地址".to_string())?;

    TcpStream::connect_timeout(&addr, Duration::from_millis(600))
        .map(|_| ())
        .map_err(|e| format!("TCP 连接失败: {}", e))
}

/// 出站代理不可达时给出告警文案；可达或无需探测时返回 None
///
/// 单独抽成纯函数而不是直接把判断埋进 `log::warn!`，是因为 log 的行为
/// 无法被断言——埋进去之后，这道闸就成了"看着有、其实不咬人"的恒真判据。
fn unreachable_proxy_warning(explicit: Option<&str>) -> Option<String> {
    let proxy = effective_proxy_for_probe(explicit)?;
    // 指向 cc-switch 自己端口的代理会被转成 no_proxy（直连），探测它没有意义
    if proxy_points_to_loopback(&proxy) {
        return None;
    }
    probe_proxy_endpoint(&proxy).err().map(|reason| {
        // 显式配置与系统代理的修法完全不同，说清楚是哪一种，
        // 否则用户会去改一个根本没在生效的设置项。
        let where_ = if explicit.filter(|s| !s.trim().is_empty()).is_some() {
            "「设置 → 高级 → 全局出站代理」里配置的"
        } else {
            "macOS 系统代理（cc-switch 未显式配置出站代理，会自动跟随它）"
        };
        format!(
            "[GlobalProxy] [GP-012] {where_}出站代理不可达: {}（{}）。需要出网的上游（如 OpenAI official）将全部失败并返回 502，请把该代理改成实际在监听的地址。",
            mask_url(&proxy),
            reason
        )
    })
}

/// 出站代理不可达时告警
///
/// 代理端口失效（典型场景：mihomo 换了端口，GUI 应用里冻结的环境变量没跟着变）
/// 之后，转发器会把每一个需要出网的请求都变成 502，而用户只看到客户端侧的
/// "Unknown error"。这里在配置生效的那一刻把话说清楚。
fn warn_if_proxy_unreachable(explicit: Option<&str>) {
    if let Some(message) = unreachable_proxy_warning(explicit) {
        log::warn!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 本模块改的 HTTP_PROXY / HTTPS_PROXY / ALL_PROXY 等是进程全局 env，
    // 历史上用本地 OnceLock 锁本模块内的测试，但跨模块（openclaw/codex 等
    // 改 HOME/CODEX_HOME 的测试）一起跑仍会互相串。委托到 crate 共享 env_lock。
    use crate::test_support;

    #[test]
    fn test_mask_url() {
        assert_eq!(mask_url("http://127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(
            mask_url("http://user:pass@127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            mask_url("socks5://admin:secret@proxy.example.com:1080"),
            "socks5://proxy.example.com:1080"
        );
        // 无端口的 URL 不应显示 ":?"
        assert_eq!(
            mask_url("http://proxy.example.com"),
            "http://proxy.example.com"
        );
        assert_eq!(
            mask_url("https://user:pass@proxy.example.com"),
            "https://proxy.example.com"
        );
    }

    #[test]
    fn test_mask_url_does_not_panic_on_multibyte_boundary() {
        // 一个无法被 Url::parse 解析、且在字节 20 处正好切在多字节字符中间的字符串。
        // 回归 https://github.com/farion1231/cc-switch 的 mask_url 越界 panic。
        let bad = "这是一个无效的代理地址不能解析";
        assert!(bad.len() > 20 && !bad.is_char_boundary(20));
        let masked = mask_url(bad);
        assert!(masked.ends_with("..."));
    }

    #[test]
    fn test_build_client_direct() {
        let result = build_client(None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_http_proxy() {
        let result = build_client(Some("http://127.0.0.1:7890"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_with_socks5_proxy() {
        let result = build_client(Some("socks5://127.0.0.1:1080"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_build_client_invalid_url() {
        // reqwest::Proxy::all 对某些无效 URL 不会立即报错
        // 使用明确无效的 scheme 来触发错误
        let result = build_client(Some("invalid-scheme://127.0.0.1:7890"));
        assert!(result.is_err(), "Should reject invalid proxy scheme");
    }

    #[test]
    fn test_proxy_points_to_loopback() {
        // 设置 CC Switch 代理端口为 15721（默认值）
        set_proxy_port(15721);

        // 只有指向 CC Switch 自己端口的 loopback 地址才返回 true
        assert!(proxy_points_to_loopback("http://127.0.0.1:15721"));
        assert!(proxy_points_to_loopback("socks5://localhost:15721"));
        assert!(proxy_points_to_loopback("127.0.0.1:15721"));

        // 其他 loopback 端口不应该被跳过（允许使用其他本地代理工具）
        assert!(!proxy_points_to_loopback("http://127.0.0.1:7890"));
        assert!(!proxy_points_to_loopback("socks5://localhost:1080"));

        // 非 loopback 地址不应该被跳过
        assert!(!proxy_points_to_loopback("http://192.168.1.10:7890"));
        assert!(!proxy_points_to_loopback("http://192.168.1.10:15721"));
    }

    #[test]
    fn test_system_proxy_points_to_loopback() {
        let _guard = test_support::env_guard();

        // 设置 CC Switch 代理端口
        set_proxy_port(15721);

        let keys = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];

        for key in &keys {
            std::env::remove_var(key);
        }

        // 指向 CC Switch 端口的代理应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:15721");
        assert!(system_proxy_points_to_loopback());

        // 指向其他端口的本地代理不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:7890");
        assert!(!system_proxy_points_to_loopback());

        // 非 loopback 地址不应该被跳过
        std::env::set_var("HTTP_PROXY", "http://10.0.0.2:7890");
        assert!(!system_proxy_points_to_loopback());

        for key in &keys {
            std::env::remove_var(key);
        }
    }

    // ---- 2026-10-10：OpenAI official 全量 502 的判据 ----
    //
    // 事故形态：GUI 进程环境里冻结着一个已经没人监听的代理端口（127.0.0.1:7897），
    // 上游换成需要出网的 api.openai.com 之后，每个请求都变成 502，
    // 而日志与 DB 里只有一句 "error sending request"。

    /// 串行化本模块所有会**借用并持有**监听端口的测试
    ///
    /// `live_listener()` 从固定区间顺序扫描，两个并发测试会挑到**同一个**
    /// 端口。这把锁同时挡住 `commands::global_proxy` 里那个同样借端口的
    /// 测试——模块内私有的锁挡不住跨模块串扰，所以委托到 crate 共享锁。
    fn port_churn_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::port_churn_guard()
    }

    /// 借一个"此刻确实有人监听"的端口，并把绑定的 listener 一起交回
    ///
    /// 测试要分别验证「监听中」和「没人监听」两侧，所以这里不能先放开。
    /// 用非临时端口区间（< 49152）：临时端口区间同时是本机出站连接的源端口，
    /// 借来就意味着端口随时会被别人占掉。
    fn live_listener() -> (u16, std::net::TcpListener) {
        for port in 30000u16..30100 {
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                return (port, listener);
            }
        }
        panic!("30000-30100 区间内找不到空闲端口");
    }

    /// 取一个当场可证明"没人监听"的 loopback 地址
    ///
    /// ## 为什么不能靠"释放一个本地端口"来制造死端口
    ///
    /// 这条路走过两个坑，都实测复现过：
    ///
    /// 1. `bind("127.0.0.1:0")` 借来的**临时端口**（区间 49152–65535，
    ///    `sysctl net.inet.ip.portrange`）同时是本机出站连接的源端口。
    ///    本机 ~1485 个 TCP socket 在 churn，一条新建的出站连接就可能
    ///    正好占住刚释放的那个端口，`connect()` 遂成功。
    /// 2. 换成扫固定区间（30000–30100）也不可靠：实测仍有约 5/8 的失败率，
    ///    失败现场是 `connect` 成功而 `lsof -sTCP:LISTEN` 查不到任何监听者——
    ///    说明占用就发生在探测的那一瞬间。
    ///
    /// 与其继续和端口分配器赛跑，不如挑一个**普通进程根本抢不走**的端口：
    /// 1/7/9 是特权端口，bind 需要 root，测试进程永远占不了它们。
    /// 再当场复核一次"确实被拒绝"，万一环境里真有服务在听就显式失败，
    /// 而不是悄悄放过一个假的死端口。
    fn dead_endpoint() -> String {
        const CANDIDATES: [u16; 3] = [1, 7, 9];
        for port in CANDIDATES {
            let url = format!("http://127.0.0.1:{port}");
            if probe_proxy_endpoint(&url).is_err() {
                return url;
            }
        }
        panic!("特权端口 1/7/9 都没被拒绝：本机有服务在监听，夹具前提不成立");
    }

    #[tokio::test]
    async fn describe_transport_error_exposes_real_cause() {
        let dead = dead_endpoint();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(dead.clone()).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let err = client
            .get("https://api.openai.com/v1/responses")
            .send()
            .await
            .expect_err("连没人监听的代理端口必须失败");

        let desc = describe_transport_error(&err);

        // 旧实现（只取 without_url）产出的就是这一句
        assert_ne!(
            desc, "error sending request",
            "描述退化成无信息字符串：{desc}"
        );
        let lower = desc.to_lowercase();
        assert!(
            lower.contains("refused") || lower.contains("connect"),
            "描述里必须带出可行动的真实原因，实际: {desc}"
        );
    }

    #[test]
    fn strip_urls_does_not_leak_credentials() {
        let out = strip_urls(
            "error sending request for url (https://api.openai.com/v1/responses?key=sk-secret-value)",
        );
        assert!(
            !out.contains("sk-secret-value"),
            "凭据不得留在错误文本: {out}"
        );
        assert!(out.contains("<url>"), "应保留占位符: {out}");
    }

    #[test]
    fn strip_urls_keeps_plain_text_intact() {
        assert_eq!(
            strip_urls("TCP 连接失败: Connection refused"),
            "TCP 连接失败: Connection refused"
        );
        // 末尾无斜杠的裸域名不属于 URL，不应被替换
        assert_eq!(
            strip_urls("failed to reach api.openai.com"),
            "failed to reach api.openai.com"
        );
    }

    #[test]
    fn probe_proxy_endpoint_distinguishes_listening_from_dead() {
        let _churn = port_churn_guard();
        // 自证：量具本身必须能区分「有人监听」与「没人监听」。
        let (live_port, live) = live_listener();

        let ok = probe_proxy_endpoint(&format!("http://127.0.0.1:{live_port}"));
        assert!(ok.is_ok(), "监听中的端口必须判定为可达，实际: {ok:?}");

        drop(live);
        // 「没人监听」那一侧不能用"刚释放的本机端口"来制造——本机端口会被
        // 出站连接抢走，实测这个断言会间歇性红。改用特权端口（见 dead_endpoint）。
        let dead = dead_endpoint();
        let err = probe_proxy_endpoint(&dead).expect_err("没人监听的端口必须判定为不可达");
        assert!(
            err.contains("TCP 连接失败"),
            "不可达原因应可行动，实际: {err}"
        );
    }

    #[test]
    fn effective_proxy_prefers_explicit_over_env() {
        let _guard = test_support::env_guard();
        // 必须清空全部代理变量：测试进程继承真实 shell 的 HTTP_PROXY，
        // 只删 HTTPS_PROXY 会让断言读到外面那一份，读数就不由夹具决定了。
        let keys = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        for key in &keys {
            std::env::remove_var(key);
        }

        // 复现事故取值：一个已经失效的陈旧端口
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:7897");

        assert_eq!(
            effective_proxy_for_probe(Some("http://127.0.0.1:7890")).as_deref(),
            Some("http://127.0.0.1:7890"),
            "显式配置必须优先于环境变量"
        );
        assert_eq!(
            effective_proxy_for_probe(None).as_deref(),
            Some("http://127.0.0.1:7897"),
            "无显式配置时必须回落到环境变量（与 build_client 同口径）"
        );

        // HTTPS_PROXY 优先于 HTTP_PROXY
        std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1080");
        assert_eq!(
            effective_proxy_for_probe(None).as_deref(),
            Some("http://127.0.0.1:7897"),
            "scheme 专用变量应优先于 HTTP_PROXY"
        );

        for key in &keys {
            std::env::remove_var(key);
        }
        // 注意这里必须用 effective_proxy_from(None, None) 而不是
        // effective_proxy_for_probe(None)：后者会去读真实 macOS 系统代理，
        // 而这台机器上它确实开着（192.168.31.30:7897），断言就不是由夹具决定了。
        assert_eq!(effective_proxy_from(None, None), None);
    }

    // ---- 2026-10-14：Codex 全量 502 的判据 ----
    //
    // 事故形态：cc-switch 没有配任何显式出站代理，GUI 进程里也没有代理环境变量，
    // 但 macOS 系统代理开着（192.168.31.30:7897）。reqwest 会去读系统代理，
    // 于是每个上游请求都在隧道里走它；系统代理一抖（Connection refused /
    // DNS 失败），就全量 502。旧实现只查环境变量，于是：
    //   - 日志打印 "direct connection"（假话）
    //   - GP-012 探测不到真正在用的地址（恒真告警）
    //   - 防自环闸门 system_proxy_points_to_loopback() 同样看不见它

    #[test]
    fn parse_scutil_proxy_reads_https_system_proxy() {
        let output = "<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7897
  HTTPProxy : 192.168.31.30
  HTTPSEnable : 1
  HTTPSPort : 7897
  HTTPSProxy : 192.168.31.30
  SOCKSEnable : 1
  SOCKSPort : 7900
  SOCKSProxy : 127.0.0.1
}";
        assert_eq!(
            parse_scutil_proxy(output).as_deref(),
            Some("http://192.168.31.30:7897")
        );
    }

    #[test]
    fn parse_scutil_proxy_falls_back_to_http_when_https_disabled() {
        let output = "<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7897
  HTTPProxy : 192.168.31.30
  HTTPSEnable : 0
  HTTPSProxy : 192.168.31.30
  HTTPSPort : 7897
}";
        assert_eq!(
            parse_scutil_proxy(output).as_deref(),
            Some("http://192.168.31.30:7897"),
            "HTTPS 关掉时应回落 HTTP 代理"
        );
    }

    #[test]
    fn parse_scutil_proxy_returns_none_when_nothing_enabled_or_pac_only() {
        // 全关 = 没有系统代理
        let off = "<dictionary> {
  HTTPEnable : 0
  HTTPSEnable : 0
}";
        assert_eq!(parse_scutil_proxy(off), None);

        // PAC 无法还原成静态地址，返回 None 而不是硬凑一个
        let pac = "<dictionary> {
  HTTPEnable : 1
  HTTPPort : 7897
  HTTPProxy : 192.168.31.30
  ProxyAutoConfigEnable : 1
  ProxyAutoConfigURLString : http://pac.example.com/proxy.pac
}";
        assert_eq!(parse_scutil_proxy(pac), None, "PAC 不该被硬解析成静态代理");
    }

    #[test]
    fn effective_proxy_falls_back_to_os_system_proxy() {
        let _guard = test_support::env_guard();
        let keys = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"];
        for key in &keys {
            std::env::remove_var(key);
        }

        // 无显式配置、无环境变量时，真正生效的是 macOS 系统代理。
        // 旧实现在这里返回 None，于是 GP-012 对真实事故形态完全失明。
        assert_eq!(
            effective_proxy_from(None, Some("http://192.168.31.30:7897")).as_deref(),
            Some("http://192.168.31.30:7897"),
            "系统代理必须被认作生效出口，否则告警与防自环闸门都是恒真的"
        );

        // 环境变量仍然优先于系统代理（与 hyper-util from_system 的顺序一致）
        std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:7897");
        assert_eq!(
            effective_proxy_from(None, Some("http://192.168.31.30:7897")).as_deref(),
            Some("http://127.0.0.1:7897"),
            "环境变量应优先于系统代理"
        );

        for key in &keys {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn effective_proxy_ignores_all_proxy_env() {
        let _guard = test_support::env_guard();
        // hyper-util 的 from_env() 只读 HTTPS_PROXY/HTTP_PROXY/NO_PROXY，
        // ALL_PROXY 根本不参与。把 ALL_PROXY 算进"生效出口"= 探测一个
        // 从没被用过的地址，告警就成了噪声。
        for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            std::env::remove_var(key);
        }
        std::env::set_var("ALL_PROXY", "http://127.0.0.1:9999");

        assert_eq!(
            effective_proxy_from(None, Some("http://192.168.31.30:7897")).as_deref(),
            Some("http://192.168.31.30:7897"),
            "ALL_PROXY 不参与 reqwest 的取值，不该被认作生效出口"
        );

        std::env::remove_var("ALL_PROXY");
    }

    #[test]
    fn describe_effective_egress_names_the_proxy_it_actually_follows() {
        let _guard = test_support::env_guard();
        for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            std::env::remove_var(key);
        }

        // 这条断言**不能恒真**：解析器若在真机格式上失败，os_system_proxy()
        // 会退回 None、日志会说"直连"，一个只检查"不含 direct connection"
        // 的弱断言照样绿。所以这里按真机读数分两支：
        // 本机开着系统代理就必须指名道姓，没开才允许说直连。
        let described = describe_effective_egress(None);
        match os_system_proxy() {
            Some(url) => assert!(
                described.contains(&mask_url(&url)),
                "跟随系统代理时日志必须写出那个地址，实际: {described}"
            ),
            None => assert!(
                described.contains("直连"),
                "本机确实没有任何代理时才可以说直连，实际: {described}"
            ),
        }
    }

    #[test]
    fn unreachable_proxy_warning_names_dead_endpoint_and_its_consequence() {
        set_proxy_port(15721);
        let proxy = dead_endpoint();

        let msg = unreachable_proxy_warning(Some(&proxy))
            .expect("代理没人监听时必须给出告警——否则它会变成每个请求各报一次 502");

        assert!(msg.contains(&proxy), "告警要指出是哪个代理: {msg}");
        assert!(msg.contains("502"), "告警要点明后果: {msg}");
    }

    #[test]
    fn unreachable_proxy_warning_is_silent_when_endpoint_is_live() {
        let _churn = port_churn_guard();
        let (port, _live) = live_listener();

        assert!(
            unreachable_proxy_warning(Some(&format!("http://127.0.0.1:{port}"))).is_none(),
            "代理正常时不该刷告警"
        );
    }

    #[test]
    fn unreachable_proxy_warning_skips_self_loop() {
        set_proxy_port(15721);
        // 指向 cc-switch 自己端口时会被转成 no_proxy（直连），探测它只会误报
        assert!(unreachable_proxy_warning(Some("http://127.0.0.1:15721")).is_none());
    }

    #[test]
    fn unreachable_proxy_warning_reads_env_when_no_explicit_config() {
        let _guard = test_support::env_guard();
        let keys = [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ];
        for key in &keys {
            std::env::remove_var(key);
        }
        set_proxy_port(15721);

        // 未配置显式代理时，告警必须回落到环境变量里的那个地址，
        // 否则事故形态（只有环境变量有陈旧代理）恰恰是告警盲区。
        let dead = dead_endpoint();
        std::env::set_var("HTTPS_PROXY", &dead);

        let msg = unreachable_proxy_warning(None).expect("只配在环境变量里的失效代理同样必须告警");
        assert!(msg.contains(&dead), "告警要指出是哪个代理: {msg}");

        for key in &keys {
            std::env::remove_var(key);
        }
    }
}
