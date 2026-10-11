"""Round 2 验证：修复点复查 + URL 公网收敛路径验证。"""
import json, time
from playwright.sync_api import sync_playwright

BASE = "http://192.168.4.55/_console/"
USER, PASS = "jeffrey", "TestLogin2026"
OUT = "/home/jeffrey/works/showroom/showroom-web/e2e-shots/review"
checks = {}

def shot(page, name, full=False):
    page.screenshot(path=f"{OUT}/{name}.png", full_page=full)
    print(f"  shot: {name}")

with sync_playwright() as pw:
    b = pw.chromium.launch()
    pg = b.new_page(viewport={"width": 1280, "height": 900})
    pg.goto(BASE, wait_until="networkidle")
    pg.fill("#lg-user", USER); pg.fill("#lg-pass", PASS); pg.click("#lg-submit")
    pg.wait_for_selector("#app:not(.hidden)", timeout=15000)
    pg.wait_for_function("document.getElementById('proj-fb')?.textContent.includes('已加载')", timeout=15000)
    time.sleep(0.5)

    # ---- 修复1：更多菜单完整弹出 ----
    all_rows = pg.query_selector_all("#proj-tbl tbody tr[data-row]")
    more = all_rows[-1].query_selector("button[data-act=more]") if all_rows else None
    more.click(); time.sleep(0.3)
    checks["menu_items_visible"] = pg.evaluate("""() => {
      const menu = document.querySelector('#proj-tbl .more-wrap .menu:not(.hidden)');
      if (!menu) return null;
      const items = [...menu.querySelectorAll('button')].map(b => b.textContent.trim());
      const r = menu.getBoundingClientRect();
      // 视口内逐项命中检测：中心点被遮挡即裁剪
      const visible = items.map(t => {
        const btn = [...menu.querySelectorAll('button')].find(b => b.textContent.trim() === t);
        const br = btn.getBoundingClientRect();
        const hit = document.elementFromPoint(br.x + br.width/2, br.y + br.height/2);
        return [t, btn.contains(hit) || hit === btn];
      });
      return { items, menuBottom: r.bottom, visible };
    }""")
    shot(pg, "r2-01-proj-more-open")
    pg.mouse.click(640, 860); time.sleep(0.2)

    # ---- 展项页常态（tbl-page 容器边框仍在？抽屉正常？）----
    rows = pg.query_selector_all("#proj-tbl tbody tr[data-row]")
    if rows:
        rows[0].query_selector("td:last-child button").click(); time.sleep(0.8)
        shot(pg, "r2-01b-proj-drawer")
        pg.click("#drawer-close"); time.sleep(0.3)
    shot(pg, "r2-01-proj")

    # ---- 修复2：数据页 grid 横排 ----
    pg.click('#tabs button[data-tab="db"]'); time.sleep(0.8)
    checks["db_grid_flow"] = pg.evaluate("""() => {
      const g = document.querySelector('#tab-db .grid');
      return { display: getComputedStyle(g).display, flexDir: getComputedStyle(g).flexDirection };
    }""")
    shot(pg, "r2-04-db", full=True)

    # ---- 管理页 policy grid ----
    pg.click('#tabs button[data-tab="admin"]'); time.sleep(1.2)
    checks["admin_grid_flow"] = pg.evaluate("""() => {
      const g = document.querySelector('#tab-admin .grid');
      return g ? getComputedStyle(g).display : 'no grid';
    }""")
    shot(pg, "r2-06-admin", full=True)

    # ---- 修复3：帮助表 wrap ----
    pg.click('#tabs button[data-tab="help"]'); time.sleep(0.6)
    checks["help_td_wrap"] = pg.evaluate("""() => {
      const tds = [...document.querySelectorAll('#tab-help tbody td.wrap')];
      const t = tds[0];
      return { wrapCount: tds.length, whiteSpace: t ? getComputedStyle(t).whiteSpace : null,
               overflowWrap: t ? getComputedStyle(t).overflowWrap : null };
    }""")
    shot(pg, "r2-05-help", full=True)

    # ---- 修复4/5/6：字号、token、原生控件 ----
    checks["h2_size"] = pg.evaluate("getComputedStyle(document.querySelector('h2')).fontSize")
    checks["non_token_hex"] = pg.evaluate("""() => {
      const css = document.querySelector('style').textContent;
      const rootBlock = css.match(/:root\\s*{[^}]*}/)?.[0] || '';
      const defs = new Set([...rootBlock.matchAll(/#[0-9a-fA-F]{3,8}\\b/g)].map(m => m[0].toLowerCase()));
      const used = new Set([...css.matchAll(/#[0-9a-fA-F]{3,8}\\b/g)].map(m => m[0].toLowerCase()));
      return [...used].filter(h => !defs.has(h));
    }""")
    checks["file_btn_colors"] = pg.evaluate("""() => {
      const el = document.getElementById('pub-file');
      return null; // 伪元素取不到 file input（发布页当前 hidden），切换后再取
    }""")
    pg.click('#tabs button[data-tab="pub"]'); time.sleep(0.5)
    checks["file_btn_bg"] = pg.evaluate("getComputedStyle(document.getElementById('pub-file'), '::file-selector-button').backgroundColor")
    checks["placeholder_color"] = pg.evaluate("getComputedStyle(document.getElementById('pub-tag'), '::placeholder').color")
    checks["progress_accent"] = pg.evaluate("getComputedStyle(document.getElementById('dp-prog')).accentColor")
    checks["checkbox_accent"] = pg.evaluate("""() => { const c = document.getElementById('pub-src-tar'); return getComputedStyle(c).accentColor; }""")
    shot(pg, "r2-02-pub")

    # ---- 修复7：URL 公网收敛路径 ----
    pg.click('#tabs button[data-tab="proj"]'); time.sleep(0.6)
    pg.click("#btn-settings"); time.sleep(0.4)
    pg.fill("#set-base", "http://47.107.155.180:10100")
    pg.dispatch_event("#set-base", "input"); time.sleep(0.4)
    checks["public_base_saved"] = pg.evaluate("localStorage.getItem('sr.publicBase')")
    checks["base_note_hidden_after"] = pg.evaluate("document.getElementById('proj-base-note').classList.contains('hidden')")
    checks["first_row_url"] = pg.evaluate("document.querySelector('#proj-tbl tbody tr[data-row] td.wrap a')?.textContent")
    pg.keyboard.press("Escape"); time.sleep(0.3)
    shot(pg, "r2-01c-proj-public-url")
    # 清掉，恢复兜底态
    pg.click("#btn-settings"); time.sleep(0.3)
    pg.fill("#set-base", ""); pg.dispatch_event("#set-base", "input"); time.sleep(0.3)
    checks["base_cleared"] = pg.evaluate("localStorage.getItem('sr.publicBase')")
    pg.keyboard.press("Escape")

    # ---- B 类复测（回归确认）----
    checks["regression"] = pg.evaluate("""() => {
      const badPh = [];
      for (const el of document.querySelectorAll('input:not([type=hidden]):not([type=radio]):not([type=checkbox]), textarea')) {
        const ph = el.getAttribute('placeholder');
        if (ph && ph !== '' && !ph.endsWith('…')) badPh.push(el.id);
      }
      return { badPh, bodyTouch: getComputedStyle(document.body).touchAction,
               fv: (() => { const b = document.querySelector('#tabs button'); b.focus(); return b.matches(':focus-visible'); })() };
    }""")

    b.close()

print(json.dumps(checks, ensure_ascii=False, indent=1))
