import json
import time

import requests
import websocket
from bs4 import BeautifulSoup

url = "https://courses.sjtu.edu.cn/app/oauth/2.0/login?login_type=outer"
r = requests.get(url, headers={"accept-language": "zh-CN"})
cookies = r.cookies
for h in r.history:
    cookies.update(h.cookies)
uuid = BeautifulSoup(r.content, "html.parser").find("a", attrs={"id": "firefox_link"})["href"].split("=")[1]
print("uuid", uuid)
print("all cookies", cookies.get_dict())
cookie = "; ".join(f"{k}={v}" for k, v in cookies.get_dict().items())
print("full cookie header len", len(cookie))

for label, cookie in [
    ("all", "; ".join(f"{k}={v}" for k, v in cookies.get_dict().items())),
    ("jaccount", "; ".join(f"{k}={v}" for k, v in cookies.get_dict(domain="jaccount.sjtu.edu.cn").items())),
]:
    print("try", label, len(cookie))
    ws = websocket.create_connection(
        f"wss://jaccount.sjtu.edu.cn/jaccount/sub/{uuid}",
        header={"cookie": cookie, "Origin": "https://jaccount.sjtu.edu.cn"},
    )
    ws.settimeout(3)
    ws.send('{ "type": "UPDATE_QR_CODE" }')
    try:
        msg = ws.recv()
        print(label, "recv", msg[:120])
    except Exception as e:
        print(label, "error", e)
    ws.close()
