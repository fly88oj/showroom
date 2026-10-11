"""Round 1 审查：截图全部标签页 + DOM 级规范检查。"""
import json, sys, time
from playwright.sync_api import sync_playwright

BASE = "http://192.168.4.55/_console/"
USER, PASS = "jeffrey", "TestLogin2026"
OUT = "/home/jeffrey/works/showroom/showroom-web/e2e-shots/review"
issues = []
checks = {}

def shot(page, name, full=False):
    page.screenshot(path=f"{OUT}/{name}.png", full_page=full)
    print(f"  shot: {name}")

with sync_playwright() as pw:
    b = pw.chromium.launch()
    pg = b.new_page(viewport={"width": 1280, "height": 900})
    pg.goto(BASE, wait_until="networkidle")
    shot(pg, "r1-00-login")

    # 登录
    pg.fill("#lg-user", USER)
    pg.fill("#lg-pass", PASS)
    pg.click("#lg-submit")
    pg.wait_for_selector("#app:not(.hidden)", timeout=15000)
    pg.wait_for_function("document.getElementById('proj-fb')?.textContent.includes('已加载') || document.getElementById('proj-fb')?.textContent.includes('加载失败')", timeout=15000)
    time.sleep(0.6)

    # ---- 展项页 ----
    shot(pg, "r1-01-proj")
    # 行抽屉
    rows = pg.query_selector_all("#proj-tbl tbody tr[data-row]")
    checks["proj_rows"] = len(rows)
    if rows:
        rows[0].query_selector("button[data-act=logs], button[data-act=more]").click() if False else None
        # 点详情 ▾（最后一列）
        dg = rows[0].query_selector("td:last-child button")
        if dg:
            dg.click(); time.sleep(0.8)
            shot(pg, "r1-01b-proj-drawer")
            pg.click("#drawer-close"); time.sleep(0.3)
    # 更多菜单
    more = pg.query_selector("#proj-tbl button[data-act=more]")
    if more:
        more.click(); time.sleep(0.3)
        shot(pg, "r1-01c-proj-more")
        pg.keyboard.press("Escape")  # 菜单不响应 Esc 也无妨（点击别处收起）
        pg.mouse.click(640, 850); time.sleep(0.2)

    # ---- 发布页 ----
    pg.click('#tabs button[data-tab="pub"]'); time.sleep(0.8)
    shot(pg, "r1-02-pub")

    # ---- 镜像库 ----
    pg.click('#tabs button[data-tab="img"]'); time.sleep(1.0)
    shot(pg, "r1-03-img")

    # ---- 数据 ----
    pg.click('#tabs button[data-tab="db"]'); time.sleep(0.8)
    shot(pg, "r1-04-db", full=True)

    # ---- 帮助 ----
    pg.click('#tabs button[data-tab="help"]'); time.sleep(0.8)
    shot(pg, "r1-05-help", full=True)

    # ---- 管理（若存在）----
    admin_btn = pg.query_selector('#tabs button[data-tab="admin"]')
    checks["admin_tab"] = bool(admin_btn)
    if admin_btn:
        admin_btn.click(); time.sleep(1.5)
        shot(pg, "r1-06-admin", full=True)

    # ---- 设置对话框（Esc 关闭 + 焦点归还）----
    pg.click('#tabs button[data-tab="proj"]'); time.sleep(0.5)
    pg.click("#btn-settings"); time.sleep(0.5)
    shot(pg, "r1-07-settings")
    checks["dlg_esc_close"] = pg.evaluate("""() => {
      const d = document.getElementById('dlg-settings');
      d.dispatchEvent(new KeyboardEvent('keydown', {key:'Escape', bubbles:true, cancelable:true}));
      return d.open;
    }""")
    pg.keyboard.press("Escape"); time.sleep(0.3)
    checks["dlg_closed_after_esc"] = not pg.evaluate("document.getElementById('dlg-settings').open")
    checks["focus_back"] = pg.evaluate("document.activeElement?.id || document.activeElement?.tagName")

    # ================= DOM 级规范检查 =================
    checks["font_scale"] = pg.evaluate("""() => {
      const cs = (sel, prop='fontSize') => { const el = document.querySelector(sel); return el ? getComputedStyle(el)[prop] : null; };
      return { h1: cs('h1'), h2: cs('h2'), h3: cs('h3'), body: cs('body'),
               note: cs('.note'), mono: cs('.mono'), fieldLabel: cs('.field > span'),
               h2w: cs('h2','fontWeight'), h3w: cs('h3','fontWeight') };
    }""")

    # 字色 token：收集样式表里所有 hex（区别 token 定义行）
    checks["hex_colors_in_css"] = pg.evaluate("""() => {
      const css = document.querySelector('style').textContent;
      const defs = new Set();
      const rootBlock = css.match(/:root\\s*{[^}]*}/)?.[0] || '';
      for (const m of rootBlock.matchAll(/#[0-9a-fA-F]{3,8}\\b/g)) defs.add(m[0].toLowerCase());
      const used = new Set();
      for (const m of css.matchAll(/#[0-9a-fA-F]{3,8}\\b/g)) used.add(m[0].toLowerCase());
      return { tokenDefs: [...defs], nonToken: [...used].filter(h => !defs.has(h)) };
    }""")

    # 表单控件 label 关联 + placeholder 以 … 结尾
    checks["form_audit"] = pg.evaluate("""() => {
      const badLabel = [], badPh = [], phList = [];
      for (const el of document.querySelectorAll('input:not([type=hidden]):not([type=radio]):not([type=checkbox]), select, textarea')) {
        if (!el.id) { badLabel.push('(no-id) ' + el.outerHTML.slice(0,80)); continue; }
        const labeled = document.querySelector(`label[for="${CSS.escape(el.id)}"]`) || el.closest('label');
        if (!labeled && !el.getAttribute('aria-label') && !el.getAttribute('aria-labelledby')) badLabel.push(el.id);
        const ph = el.getAttribute('placeholder');
        if (el.tagName !== 'SELECT') phList.push([el.id, ph]);
        if (ph !== null && ph !== '' && !ph.endsWith('…')) badPh.push([el.id, ph]);
      }
      return { badLabel, badPh, phCount: phList.length };
    }""")

    # radio/checkbox label 关联
    checks["choice_audit"] = pg.evaluate("""() => {
      const bad = [];
      for (const el of document.querySelectorAll('input[type=radio], input[type=checkbox]')) {
        const ok = document.querySelector(`label[for="${CSS.escape(el.id)}"]`) || el.closest('label') || el.getAttribute('aria-label');
        if (!ok) bad.push(el.id || el.name);
      }
      return bad;
    }""")

    # 滚动容器 overscroll / 触摸
    checks["overscroll_touch"] = pg.evaluate("""() => {
      const o = [];
      for (const el of document.querySelectorAll('.tbl-wrap, pre.out')) {
        const s = getComputedStyle(el);
        if (s.overscrollBehavior !== 'contain') o.push([el.className, s.overscrollBehavior]);
      }
      return { badOverscroll: o, bodyTouch: getComputedStyle(document.body).touchAction,
               btnTouch: getComputedStyle(document.querySelector('#tabs button')).touchAction };
    }""")

    # 数字列 tabular-nums
    checks["tabular"] = pg.evaluate("""() => {
      const td = document.querySelector('#proj-tbl td.num, td.num');
      return td ? getComputedStyle(td).fontVariantNumeric : 'no .num cell';
    }""")

    # 长内容 break
    checks["word_break"] = pg.evaluate("""() => {
      const td = document.querySelector('td.wrap');
      return td ? getComputedStyle(td).wordBreak + '/' + getComputedStyle(td).whiteSpace : 'no td.wrap';
    }""")

    # 按钮 hover 效果
    pg.hover("#btn-settings")
    checks["btn_hover_bg"] = pg.evaluate("getComputedStyle(document.getElementById('btn-settings')).backgroundColor")
    checks["btn_rest_bg"] = pg.evaluate("""() => { const b=document.createElement('button'); b.className=''; document.body.append(b);
      const c=getComputedStyle(b).backgroundColor; b.remove(); return c; }""")

    # focus-visible 键盘可达
    pg.keyboard.press("Tab"); pg.keyboard.press("Tab")
    checks["focus_visible"] = pg.evaluate("""() => {
      const el = document.activeElement;
      const s = getComputedStyle(el);
      return { id: el.id || el.textContent.slice(0,10), outline: s.outlineWidth + ' ' + s.outlineStyle + ' ' + s.outlineColor,
               isFv: el.matches(':focus-visible') };
    }""")

    # 状态列文本（中文友好词？）与 URL、时间格式
    checks["proj_first_row"] = pg.evaluate("""() => {
      const tr = document.querySelector('#proj-tbl tbody tr[data-row]');
      if (!tr) return null;
      return { status: tr.children[1]?.textContent.trim(), statusTitle: tr.children[1]?.getAttribute('title'),
               url: tr.children[2]?.textContent.trim(), time: tr.children[4]?.textContent.trim() };
    }""")
    checks["base_note_visible"] = pg.evaluate("!document.getElementById('proj-base-note').classList.contains('hidden')")
    checks["publicBase_saved"] = pg.evaluate("localStorage.getItem('sr.publicBase')")

    # 展项页展开一个行抽屉后 reduced-motion 动画检查（若有重建中）
    checks["reduced_motion_css"] = pg.evaluate("""() => {
      for (const sheet of document.styleSheets) {
        for (const r of sheet.cssRules) {
          if (r.media && /prefers-reduced-motion/.test(r.media.mediaText)) return r.media.mediaText + ' -> ' + r.cssRules.length + ' rules';
        }
      }
      return 'none';
    }""")

    b.close()

print(json.dumps(checks, ensure_ascii=False, indent=1))
