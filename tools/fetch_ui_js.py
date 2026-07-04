import re
import requests

base = "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/"
html = requests.get(base, timeout=20).text
scripts = re.findall(r'src="([^"]+\.js)"', html)
print("scripts", len(scripts))
for rel in scripts[:12]:
    url = rel if rel.startswith("http") else requests.compat.urljoin(base, rel)
    print("fetch", url)
    js = requests.get(url, timeout=30).text
    for kw in ["decrypt", "解密", "getVodVideoInfos", "AES", "CryptoJS", "jwt_token"]:
        if kw.lower() in js.lower() or kw in js:
            print("  hit", kw, "in", rel)
