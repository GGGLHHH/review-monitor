//! 登录凭证：从登录网页里取出的 cookie（含 httpOnly 的 bsid）和那个网页的 User-Agent。
//!
//! 登录完成后宿主把它交给 Poller，之后所有请求都由程序直接带着它发，网页就可以关掉了。
//! 接口响应里的 Set-Cookie 会更新它，和浏览器的行为一致，免得会话因为 cookie 没续上而提前失效。
//! 存在 data.db 的 state 表里；Windows 上先用 DPAPI 加密（只有当前 Windows 用户能解开），
//! 安卓的数据目录本身是应用私有的，不另外加密。
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub cookies: BTreeMap<String, String>,
    #[serde(default)]
    pub user_agent: String,
}

impl Credentials {
    /// 浏览器形式的 Cookie 头：`a=1; b=2`
    pub fn from_header(header: &str, user_agent: &str) -> Self {
        let cookies = header.split(';')
            .filter_map(|p| p.trim().split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        Self { cookies, user_agent: user_agent.trim().to_string() }
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    pub fn header(&self) -> String {
        self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    /// 按一条 Set-Cookie 更新；返回是否有变化。过期（Max-Age≤0 或 Expires 已过）的删掉。
    pub fn apply_set_cookie(&mut self, line: &str, now_ms: i64) -> bool {
        let mut parts = line.split(';');
        let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else { return false };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() { return false; }
        let mut expired = false;
        for a in parts {
            let (k, v) = a.split_once('=').unwrap_or((a, ""));
            match k.trim().to_ascii_lowercase().as_str() {
                "max-age" => if v.trim().parse::<i64>().is_ok_and(|s| s <= 0) { expired = true },
                "expires" => {
                    if let Ok(t) = chrono::DateTime::parse_from_rfc2822(v.trim()) && t.timestamp_millis() <= now_ms {
                        expired = true;
                    }
                }
                _ => {}
            }
        }
        if expired {
            return self.cookies.remove(name).is_some();
        }
        self.cookies.insert(name.to_string(), value.to_string()).as_deref() != Some(value)
    }

    pub fn seal(&self) -> String {
        hex(&sys::protect(serde_json::to_string(self).unwrap_or_default().as_bytes()))
    }

    /// 解不开（换了 Windows 用户、数据损坏）就当没有凭证，重新登录即可
    pub fn unseal(s: &str) -> Option<Self> {
        let raw = unhex(s)?;
        serde_json::from_slice(&sys::unprotect(&raw)?).ok()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(windows)]
mod sys {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN};

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { let _ = LocalFree(Some(HLOCAL(out.pbData.cast()))); }
        v
    }

    pub fn protect(data: &[u8]) -> Vec<u8> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        match unsafe { CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) } {
            Ok(()) => take(out),
            Err(e) => { tracing::error!("DPAPI 加密失败: {e}"); Vec::new() }
        }
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe { CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }.ok()?;
        Some(take(out))
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn protect(data: &[u8]) -> Vec<u8> {
        data.to_vec()
    }
    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        Some(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let c = Credentials::from_header(" com.sankuai.meishi.fe.kdb-bsid=abc=; _lxsdk=1 ;bad; =x", "UA/1");
        assert_eq!(c.cookies.len(), 2);
        assert_eq!(c.cookies["com.sankuai.meishi.fe.kdb-bsid"], "abc=", "值里的 = 要保留");
        assert_eq!(c.header(), "_lxsdk=1; com.sankuai.meishi.fe.kdb-bsid=abc=");
        assert_eq!(c.user_agent, "UA/1");
        assert!(Credentials::from_header("", "").is_empty());
    }

    #[test]
    fn set_cookie_updates_and_expires() {
        let mut c = Credentials::from_header("a=1; b=2", "");
        assert!(c.apply_set_cookie("a=9; Path=/; Domain=.meituan.com; HttpOnly", 0));
        assert!(!c.apply_set_cookie("a=9; Path=/", 0), "没变化");
        assert!(c.apply_set_cookie("c=3; Max-Age=3600", 0));
        assert!(c.apply_set_cookie("b=; Max-Age=0", 0));
        assert!(c.apply_set_cookie("c=x; Expires=Thu, 01 Jan 1970 00:00:01 GMT", 5_000));
        assert_eq!(c.header(), "a=9");
        assert!(!c.apply_set_cookie("garbage", 0));
    }

    #[test]
    fn seal_roundtrip() {
        let c = Credentials::from_header("bsid=秘密; x=1", "Mozilla/5.0");
        let s = c.seal();
        assert!(!s.contains("bsid"), "存库的是密文/十六进制，不是明文 cookie");
        assert_eq!(Credentials::unseal(&s), Some(c));
        assert_eq!(Credentials::unseal("zz"), None);
        assert_eq!(Credentials::unseal("abc"), None);
    }
}
