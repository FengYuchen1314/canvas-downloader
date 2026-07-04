import re
import requests

html = requests.get("https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/", timeout=20).text
chunks = re.findall(r"""['"]([^'"]+\.js)['"]""", html)
print("all js", chunks)
for rel in chunks:
    if "index-" in rel:
        continue
    url = rel if rel.startswith("http") else requests.compat.urljoin(
        "https://v.sjtu.edu.cn/jy-application-canvas-sjtu-ui/", rel
    )
    print("chunk", url)
