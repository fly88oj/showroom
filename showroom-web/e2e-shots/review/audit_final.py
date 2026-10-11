"""终态截图清单 + reduced-motion 断言。"""
import json, time
from playwright.sync_api import sync_playwright

BASE = "http://192.168.4.55/_console/"
USER, PASS = "jeffrey", "TestLogin2026"
OUT = "/home/jeffrey/works/showroom/showroom-web/e2e-shots/review"
final = {}

def shot(page, name, full=False):
    page.screenshot(path=f"{OUT}/{name}.png", full_page=full)
    print(f"  final: {name}")

with sync_playwright() as pw:
    b = pw.chromium.launch()
    pg = b.new_page(viewport={"width": 1280, "height": 900})
    pg.goto(BASE, wait_until="networkidle")
    shot(pg, "final-00-login")
    pg.fill("#lg-user", USER); pg.fill("#lg-pass", PASS); pg.click("#lg-submit")
    pg.wait_for_selector("#app:not(.hidden)", timeout=15000)
    pg.wait_for_function("document.getElementById('proj-fb')?.textContent.includes('已加载')", timeout=15000)
    time.sleep(0.8)
    shot(pg, "final-01-proj")
    rows = pg.query_selector_all("#proj-tbl tbody tr[data-row]")
    if rows:
        rows[0].query_selector("td:last-child button").click(); time.sleep(1.0)
        shot(pg, "final-01b-proj-drawer")
        pg.click("#drawer-close"); time.sleep(0.3)
    pg.click('#tabs button[data-tab="pub"]'); time.sleep(0.6)
    shot(pg, "final-02-pub")
    pg.click('#tabs button[data-tab="img"]'); time.sleep(1.0)
    shot(pg, "final-03-img")
    pg.click('#tabs button[data-tab="db"]'); time.sleep(0.8)
    shot(pg, "final-04-db", full=True)
    pg.click('#tabs button[data-tab="help"]'); time.sleep(0.6)
    shot(pg, "final-05-help", full=True)
    ab = pg.query_selector('#tabs button[data-tab="admin"]')
    if ab:
        ab.click(); time.sleep(1.5)
        shot(pg, "final-06-admin", full=True)
    pg.click('#tabs button[data-tab="proj"]'); time.sleep(0.5)
    pg.click("#btn-settings"); time.sleep(0.4)
    shot(pg, "final-07-settings")
    pg.keyboard.press("Escape"); time.sleep(0.2)

    # focus-visible 键盘导航证据
    pg.keyboard.press("Tab"); pg.keyboard.press("Tab")
    final["focus_visible"] = pg.evaluate("""() => {
      const el = document.activeElement, s = getComputedStyle(el);
      return { el: el.id || el.textContent, outline: `${s.outlineWidth} ${s.outlineStyle} ${s.outlineColor}`, fv: el.matches(':focus-visible') };
    }""")
    shot(pg, "final-08-focus-visible")
    b.close()

    # reduced-motion：呼吸动画应静止
    b = pw.chromium.launch()
    ctx = b.new_context(viewport={"width": 1280, "height": 900}, reduced_motion="reduce")
    pg = ctx.new_page()
    pg.goto(BASE, wait_until="networkidle")
    pg.fill("#lg-user", USER); pg.fill("#lg-pass", PASS); pg.click("#lg-submit")
    pg.wait_for_selector("#app:not(.hidden)", timeout=15000)
    time.sleep(0.5)
    final["reduced_motion_anim"] = pg.evaluate("""() => {
      // 注入一个 st-breath 节点验证媒体查询生效
      const s = document.createElement('span'); s.className = 'st-breath'; document.body.append(s);
      const a = getComputedStyle(s).animationName;
      s.remove();
      return a; // 'none' = 静止 ✓
    }""")
    b.close()

print(json.dumps(final, ensure_ascii=False, indent=1))
