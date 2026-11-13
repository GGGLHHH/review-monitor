# spike: 从 WebView2 profile 解出 ecom.meituan.com 的 cookie(Windows DPAPI + AES-GCM)
# 输出 JSON: {aes_key_hex, cookies: {name: value}}  —— 解密 AES-GCM 由 Node 侧完成
import ctypes, ctypes.wintypes, json, sqlite3, base64, os, sys

base = os.path.join(os.environ['APPDATA'], 'review-monitor', 'webview', 'EBWebView')
local = json.load(open(os.path.join(base, 'Local State'), encoding='utf-8'))

class DATA_BLOB(ctypes.Structure):
    _fields_ = [('cbData', ctypes.wintypes.DWORD), ('pbData', ctypes.POINTER(ctypes.c_char))]

def dpapi_unprotect(data: bytes) -> bytes:
    inp = DATA_BLOB(len(data), None)
    inp.pbData = ctypes.cast(ctypes.create_string_buffer(data, len(data)), ctypes.POINTER(ctypes.c_char))
    out = DATA_BLOB()
    if not ctypes.windll.crypt32.CryptUnprotectData(ctypes.byref(inp), None, None, None, None, 0, ctypes.byref(out)):
        raise OSError('CryptUnprotectData failed', ctypes.GetLastError())
    try:
        return ctypes.string_at(out.pbData, out.cbData)
    finally:
        ctypes.windll.kernel32.LocalFree(out.pbData)

raw = base64.b64decode(local['os_crypt']['encrypted_key'])
assert raw[:5] == b'DPAPI'
key = dpapi_unprotect(raw[5:])

con = sqlite3.connect(f'file:{os.path.join(base, "Default", "Network", "Cookies")}?mode=ro', uri=True)
out = {'aes_key_hex': key.hex(), 'cookies': {}}
for name, host, enc in con.execute("select name, host_key, cast(encrypted_value as blob) from cookies where host_key like '%meituan%'"):
    if len(enc) < 3:
        out['cookies'][name] = enc.decode('utf-8', 'replace')  # 未加密(极少)
        continue
    prefix, nonce, ct = enc[:3], enc[3:15], enc[15:]
    out['cookies'][name] = {'prefix': prefix.decode(), 'nonce': nonce.hex(), 'ct': ct.hex()}
json.dump(out, open(sys.argv[1], 'w'))
print('done,', len(out['cookies']), 'cookies')
