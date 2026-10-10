//! setup：声明式期望状态（plan-15 §2.2 / SPEC §15.1）——observe 收集现状、
//! plan_setup 纯函数出 Action、apply 逐项执行；渲染产物（policy/Caddyfile/单元/脚本）常量内嵌。
//! 纯函数（渲染/推导/幂等判定）与 subprocess 薄层（observe/apply）分区放置。

use crate::state;

// —— policy.env 渲染（plan-15 §1：setup 负责缺省生成——缺键补默认、已有键一律保留）——

/// policy 键表（§13.3/§14.4 + 存量键；§14 两键为已落语义，与 §14 实现共用）。
/// 值为固定缺省；RESERVE_MEM_MB/DISK_RESERVE_GB 首跑缺省可被 hw_defaults 覆盖（见 render_policy_hw）。
pub const POLICY_KEYS: [(&str, &str); 9] = [
    ("MAX_SLOTS", "30"),
    ("DEFAULT_MEM", "256m"),
    ("MAX_MEM", "1g"),
    ("IMPORT_MAX_GB", "8"),
    ("ALLOW_LAN_PORT", "0"),
    ("RESERVE_MEM_MB", "1536"),
    ("REDIS_DBS", "256"),
    ("DISK_RESERVE_GB", "20"),
    ("UPLOAD_SPACE_FACTOR", "4"),
];

/// 固定缺省渲染（plan-15 §2.2 签名）：已有键 verbatim 保留，缺键补默认，键序固定（幂等写盘）。
pub fn render_policy(existing: Option<&str>) -> String {
    render_policy_hw(existing, None)
}

/// hw 自适应渲染（补充需求：首跑按实测硬件推导缺省）：
/// 仅当**文件缺失**（existing=None，整文件首生成）时用 hw 值补 RESERVE_MEM_MB/DISK_RESERVE_GB；
/// 已有文件一律保留既有键、缺键补固定缺省（用户显式值永远优先）。
pub fn render_policy_hw(existing: Option<&str>, hw: Option<&HwTuning>) -> String {
    let m = existing.map(state::parse_kv).unwrap_or_default();
    let hw = if existing.is_some() { None } else { hw };
    POLICY_KEYS
        .iter()
        .map(|(k, d)| {
            let v = m.get(*k).cloned().unwrap_or_else(|| match (*k, hw) {
                ("RESERVE_MEM_MB", Some(h)) => h.reserve_mem_mb.to_string(),
                ("DISK_RESERVE_GB", Some(h)) => h.disk_reserve_gb.to_string(),
                _ => d.to_string(),
            });
            format!("{k}={v}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// —— 硬件自适应缺省（补充需求：首跑推导，纯函数可测）——

/// 首跑硬件推导产物：限额缺省 + 容器规格派生参数。
/// 规则（补充需求原文）：内存/磁盘百分比 + clamp；取整一律向下（保守侧）。
#[derive(Debug, Clone, PartialEq)]
pub struct HwTuning {
    /// clamp(总内存×15%, 1024, 6144) MB（12G≈1843；4G 兜底 1024）
    pub reserve_mem_mb: u64,
    /// clamp(总磁盘×3%, 10, 60) GB
    pub disk_reserve_gb: u64,
    /// clamp(总内存×12%, 64, 1GB) MB（向下取整）
    pub pg_shared_buffers_mb: u64,
    /// 总内存×35% MB
    pub pg_effective_cache_mb: u64,
    /// clamp(30+cores×10, 40, 120)
    pub pg_max_connections: u32,
    /// clamp(总内存×2%, 64, 512) MB
    pub redis_maxmemory_mb: u64,
}

fn clamp(v: u64, lo: u64, hi: u64) -> u64 {
    v.max(lo).min(hi)
}

/// 首跑硬件推导（纯函数；补充需求三档测试：4G 小机 / 12G 本机 / 64G 大机）。
pub fn hw_defaults(mem_mb: u64, disk_mb: u64, cores: u32) -> HwTuning {
    let disk_gb = disk_mb / 1024;
    HwTuning {
        reserve_mem_mb: clamp(mem_mb * 15 / 100, 1024, 6144),
        disk_reserve_gb: clamp(disk_gb * 3 / 100, 10, 60),
        pg_shared_buffers_mb: clamp(mem_mb * 12 / 100, 64, 1024),
        pg_effective_cache_mb: mem_mb * 35 / 100,
        pg_max_connections: u32::try_from(30 + cores.saturating_mul(10)).unwrap_or(120).clamp(40, 120),
        redis_maxmemory_mb: clamp(mem_mb * 2 / 100, 64, 512),
    }
}

/// 审计注释的硬件摘要：「12G/953G/4 核」（内存/磁盘向下取整 GB）。
pub fn hw_summary(mem_mb: u64, disk_mb: u64, cores: u32) -> String {
    format!("{}G/{}G/{} 核", mem_mb / 1024, disk_mb / 1024, cores)
}

/// unix 秒 → UTC `YYYY-MM-DD`（审计注释用；civil-from-days 算法，免引 chrono）。
pub fn date_label(unix: u64) -> String {
    let days = (unix / 86400) as i64;
    // Howard Hinnant 的 civil_from_days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // render_policy：空→全默认键；已有键保留、缺键补齐（含 §14 键位）；幂等
    #[test]
    fn render_policy_variants() {
        let fresh = render_policy(None);
        assert_eq!(
            fresh,
            "MAX_SLOTS=30\nDEFAULT_MEM=256m\nMAX_MEM=1g\nIMPORT_MAX_GB=8\nALLOW_LAN_PORT=0\n\
             RESERVE_MEM_MB=1536\nREDIS_DBS=256\nDISK_RESERVE_GB=20\nUPLOAD_SPACE_FACTOR=4\n"
        );
        let m = state::parse_kv(&render_policy(Some("MAX_SLOTS=5\nALLOW_LAN_PORT=1\n")));
        assert_eq!(m["MAX_SLOTS"], "5", "已有键保留");
        assert_eq!(m["ALLOW_LAN_PORT"], "1");
        assert_eq!(m["REDIS_DBS"], "256", "缺键补默认");
        assert_eq!((m["DISK_RESERVE_GB"].as_str(), m["UPLOAD_SPACE_FACTOR"].as_str()), ("20", "4"), "§14 键位");
        assert_eq!(m.len(), POLICY_KEYS.len());
        assert_eq!(render_policy(Some(&fresh)), fresh, "幂等：渲染产物再喂不变");
    }

    // hw 缺省只影响**首跑**（文件缺失）：已有文件的键永不覆盖
    #[test]
    fn render_policy_hw_first_run_only() {
        let hw = hw_defaults(12288, 976_128, 4);
        let m = state::parse_kv(&render_policy_hw(None, Some(&hw)));
        assert_eq!(m["RESERVE_MEM_MB"], hw.reserve_mem_mb.to_string());
        assert_eq!(m["DISK_RESERVE_GB"], hw.disk_reserve_gb.to_string());
        // 已有文件：显式值永远优先，hw 不参与
        let keep = state::parse_kv(&render_policy_hw(Some("RESERVE_MEM_MB=1536\n"), Some(&hw)));
        assert_eq!(keep["RESERVE_MEM_MB"], "1536");
        // 已有文件但缺该键：补固定缺省（非 hw——hw 只用于整文件首生成）
        let partial = state::parse_kv(&render_policy_hw(Some("MAX_SLOTS=5\n"), Some(&hw)));
        assert_eq!(partial["RESERVE_MEM_MB"], "1536");
    }

    // hw_defaults 三档（补充需求）：4G 小机 / 12G 本机 / 64G 大机
    #[test]
    fn hw_defaults_tiers() {
        // 4G 小机（2 核 / 100G 盘）：reserve 兜底 1024；disk 兜底 10；shared 491；conn 50；redis 81
        let small = hw_defaults(4096, 102_400, 2);
        assert_eq!(
            (
                small.reserve_mem_mb,
                small.disk_reserve_gb,
                small.pg_shared_buffers_mb,
                small.pg_effective_cache_mb,
                small.pg_max_connections,
                small.redis_maxmemory_mb
            ),
            (1024, 10, 491, 1433, 50, 81)
        );

        // 12G 本机（4 核 / 953G 盘）：reserve=1843（12G×15% 向下取整）
        let mid = hw_defaults(12288, 976_128, 4);
        assert_eq!(
            (
                mid.reserve_mem_mb,
                mid.disk_reserve_gb,
                mid.pg_shared_buffers_mb,
                mid.pg_effective_cache_mb,
                mid.pg_max_connections,
                mid.redis_maxmemory_mb
            ),
            (1843, 28, 1024, 4300, 70, 245)
        );

        // 64G 大机（16 核 / 4T 盘）：reserve 封顶 6144；disk 封顶 60；shared 封顶 1G；conn 封顶 120；redis 封顶 512
        let big = hw_defaults(65_536, 4_194_304, 16);
        assert_eq!(
            (
                big.reserve_mem_mb,
                big.disk_reserve_gb,
                big.pg_shared_buffers_mb,
                big.pg_effective_cache_mb,
                big.pg_max_connections,
                big.redis_maxmemory_mb
            ),
            (6144, 60, 1024, 22_937, 120, 512)
        );

        // 极小兜底：单核 512M 机器不越界
        let tiny = hw_defaults(512, 5120, 1);
        assert!(tiny.reserve_mem_mb >= 1024 && tiny.disk_reserve_gb >= 10);
        assert_eq!(tiny.pg_max_connections, 40);
        assert_eq!(tiny.pg_shared_buffers_mb, 64);
    }

    // 审计摘要与日期标签
    #[test]
    fn hw_summary_and_date() {
        assert_eq!(hw_summary(12288, 976_128, 4), "12G/953G/4 核");
        assert_eq!(hw_summary(4096, 102_400, 2), "4G/100G/2 核");
        assert_eq!(date_label(0), "1970-01-01");
        assert_eq!(date_label(1_760_117_600), "2025-10-10");
        assert_eq!(date_label(1_738_982_400), "2025-02-08");
        assert_eq!(date_label(1_709_164_800), "2024-02-29", "闰年边界");
    }
}
