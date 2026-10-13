// —— app.tsx：全局会话与数据层（真链路）——
// 会话生命周期（boot/登录/退出/心跳踢回/改密收口）+ 四路资源（projects/images/space TTL 缓存）+ 事件流订阅 + 行内在途标记。
import {
  createContext, useCallback, useContext, useEffect, useMemo, useRef, useState,
  type ReactNode,
} from "react";
import { toast } from "sonner";
import { api, consumeNdjson } from "@/lib/api";
import { ensureSeed, passwordLogin, startHeartbeat, stopHeartbeat } from "@/lib/auth";
import { dataInvalidate, dataFresh, dataMark, hydratePem, state, NAME_RE } from "@/lib/state";
import { initWasm } from "@/lib/wasm";

export type Tab = "proj" | "pub" | "img" | "db" | "help" | "admin";

export interface Project {
  project: string;
  status: string;
  version: string; // 服务端为字符串（"v7"；旧项目回落 tag）；version_seq 为数字计数
  url: string;
  updated: string;
  updatedTs: number;
  mem?: number;   // 上次发布内存（MiB；服务端 conf「256m」→ 256）
  cport?: number; // 上次发布容器端口（响应缺字段时 undefined）
}

export interface ImageItem { name: string }
export interface SpaceInfo { availGb: number; reserveGb: number }

export interface Session { user: string; admin: boolean }

/** 行内在途标记类型（§4）：发布中/上传中/60s 重建中·异常滚动窗口。 */
export type MarkKind = "publishing" | "uploading" | "rebuilding" | "failed";

// —— 三层 context 接口（useApp 聚合成既有 AppState 形状，页面零改动）——
interface SessionState {
  boot: "loading" | "login" | "app"; // wasm 装载中 / 登录窗 / 控制台
  session: Session | null;
  login: (user: string, pass: string) => Promise<void>;
  logout: () => void;
  onPwChanged: () => void;
  loginNotice: string;
}
interface DataState {
  tab: Tab;
  setTab: (t: Tab) => void;
  pubPrefill: string | null;
  setPubPrefill: (p: string | null) => void;
  projects: Project[];
  refreshProjects: (force?: boolean) => Promise<void>;
  projOp: (name: string, op: "start" | "stop" | "rm") => Promise<void>;
  rollback: (name: string, v: string) => Promise<void>;
  publishDone: (name: string) => Promise<void>;
  images: ImageItem[];
  ensureImages: (force?: boolean) => Promise<void>;
  rmImage: (name: string) => Promise<void>;
  space: SpaceInfo;
  refreshSpace: (force?: boolean) => Promise<void>;
  marks: Record<string, { kind: MarkKind; until: number }>;
  markRow: (project: string, kind: MarkKind, ms: number) => void;
}
interface EventsState {
  events: string[];
  evConnected: boolean;
}
type AppState = SessionState & DataState & EventsState;


const SessionCtx = createContext<SessionState | null>(null);
const DataCtx = createContext<DataState | null>(null);
const EventsCtx = createContext<EventsState | null>(null);

/** D1：更新列 YYYY/MM/DD HH:MM（unix 秒 → 本地时区）。 */
export const fmtTime = (unixSec: number) => {
  if (!unixSec) return "—";
  try {
    return new Intl.DateTimeFormat("zh-CN", {
      year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
    }).format(new Date(unixSec * 1000));
  } catch { return "—"; }
};

const mapProject = (p: Record<string, unknown>): Project => {
  const memRaw = String(p.mem ?? "").trim();
  const mem = /^\d+[mM]$/.test(memRaw) ? parseInt(memRaw, 10) : /^\d+$/.test(memRaw) ? Number(memRaw) : undefined;
  return {
    project: String(p.project ?? ""),
    status: String(p.status ?? ""),
    version: String(p.version ?? ""),
    url: `/${p.project}/`,
    updated: fmtTime(Number(p.updated ?? 0)),
    updatedTs: Number(p.updated ?? 0) * 1000,
    mem,
    cport: p.cport != null ? Number(p.cport) || undefined : undefined,
  };
};

export function AppProvider({ children }: { children: ReactNode }) {
  return (
    <SessionProvider>
      <DataProvider>
        <EventsProvider>{children}</EventsProvider>
      </DataProvider>
    </SessionProvider>
  );
}

/** 会话层→数据层桥（单向，仅 DataProvider 挂载后由会话层调用：登录四路并发/登出清数据）。
 *  拓扑 Session ⊃ Data ⊃ Events：Data/Events 均可消费 SessionCtx（boot 响应式），桥只解决
 *  外层 Session 触发内层 Data 的唯一缺口。 */
const dataBridge: {
  refreshProjects?: (f?: boolean) => Promise<void>;
  ensureImages?: (f?: boolean) => Promise<void>;
  refreshSpace?: (f?: boolean) => Promise<void>;
  reset?: () => void;
} = {};

// —— 会话层：boot/登录/退出/心跳/改密收口/登录框提示 ——
function SessionProvider({ children }: { children: ReactNode }) {
  const [boot, setBoot] = useState<"loading" | "login" | "app">("loading");
  const [session, setSession] = useState<Session | null>(null);
  const [loginNotice, setLoginNotice] = useState("");

  const syncAdmin = useCallback(async () => {
    const j = (await api("GET", "/_api/whoami")) as { admin?: boolean; platform_pem?: string };
    state.admin = !!j.admin;
    if (j.platform_pem && !state.pem) hydratePem(j.platform_pem);
    setSession((s) => (s ? { ...s, admin: !!j.admin } : s));
  }, []);

  const enterApp = useCallback(async () => {
    state.loggedIn = true;
    setBoot("app");
    setSession({ user: state.user, admin: state.admin });
    startHeartbeat(() => { // E_AUTH：钥匙过期/吊销 → 踢回登录窗
      state.loggedIn = false;
      setSession(null);
      setBoot("login");
    });
    // plan-17 §3：登录即 4 路并发（projects/images/space/whoami），任一路失败不阻断
    await Promise.allSettled([
      dataBridge.refreshProjects?.(true), dataBridge.ensureImages?.(true),
      dataBridge.refreshSpace?.(true), syncAdmin(),
    ]);
  }, [syncAdmin]);

  const login = useCallback(async (user: string, pass: string) => {
    setLoginNotice("");
    await passwordLogin(user.trim(), pass);
    await enterApp();
  }, [enterApp]);

  const logout = useCallback(() => {
    stopHeartbeat();
    if (state.seed && state.user && state.kid) {
      api("POST", "/_api/key-selfrm", { kid: state.kid }).catch(() => {});
    }
    state.loggedIn = false;
    state.admin = false;
    state.boundAdmin = false;
    localStorage.removeItem("sr.seed");
    localStorage.removeItem("sr.user");
    dataBridge.reset?.();
    setSession(null);
    setBoot("login");
  }, []);

  // 改密收口（设备不增终案）：远程吊销本机旧钥匙（旧记录删除）+ 清种子（重登生成新钥匙=新记录，
  // 净计数不变；刷新后无钥匙走密码表单）+ 停心跳（会话真结束，钥匙不再被后台续期）。
  const onPwChanged = useCallback(() => {
    stopHeartbeat();
    if (state.seed && state.user && state.kid) {
      api("POST", "/_api/key-selfrm", { kid: state.kid }).catch(() => {});
    }
    state.loggedIn = false;
    localStorage.removeItem("sr.seed");
    localStorage.removeItem("sr.user");
    setLoginNotice("密码已修改：请输入新密码重新登录。");
    setSession(null);
    setBoot("login");
  }, []);

  // —— 启动：wasm 装载 → 密钥无感 → 有「钥匙+账号」签名自测直达；E_AUTH 回落密码表单 ——
  useEffect(() => {
    (async () => {
      await initWasm();
      state.user = localStorage.getItem("sr.user") || "";
      ensureSeed();
      if (state.seed && NAME_RE.test(state.user)) {
        try {
          await api("GET", "/_api/health");
          await enterApp();
          return;
        } catch {
          state.user = localStorage.getItem("sr.user") || "";
        }
      }
      setBoot("login");
    })().catch(() => setBoot("login"));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const value = useMemo<SessionState>(() => ({ boot, session, login, logout, onPwChanged, loginNotice }),
    [boot, session, login, logout, onPwChanged, loginNotice]);
  return <SessionCtx.Provider value={value}>{children}</SessionCtx.Provider>;
}

// —— 数据层：四路资源（TTL 缓存）+ 导航/预填 + 行内在途标记 + 30s 轮询（最外层，无会话依赖）——

function DataProvider({ children }: { children: ReactNode }) {
  const [tab, setTabRaw] = useState<Tab>("proj");
  const [pubPrefill, setPubPrefill] = useState<string | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [images, setImages] = useState<ImageItem[]>([]);
  const [space, setSpace] = useState<SpaceInfo>({ availGb: 0, reserveGb: 0 });
  const [marks, setMarks] = useState<Record<string, { kind: MarkKind; until: number }>>({});
  const { boot } = useContext(SessionCtx)!; // 30s 轮询/断网钩子的登录响应性（boot 响应式）

  const setTab = useCallback((t: Tab) => {
    setTabRaw(t);
    if (t !== "pub") setPubPrefill(null);
  }, []);

  // —— 数据面（TTL 纪律：dataFresh/dataMark；写操作 invalidate 后强制重取）——
  const refreshProjects = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("projects"))) return;
    const j = (await api("GET", "/_api/projects")) as { projects?: Record<string, unknown>[] };
    setProjects((j.projects || []).map(mapProject));
    dataMark("projects");
  }, []);

  const refreshSpace = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("space"))) return;
    const j = (await api("GET", "/_api/space")) as { available_mb?: number; reserve_mb?: number };
    const avail = (Number(j.available_mb) - Number(j.reserve_mb)) / 1024; // 显示口径＝可用−预留（§14.1）
    if (Number.isFinite(avail)) {
      setSpace({ availGb: +(avail).toFixed(1), reserveGb: +(Number(j.reserve_mb) / 1024).toFixed(0) });
      dataMark("space");
    }
  }, []);

  const ensureImages = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("images"))) return;
    const j = (await api("GET", "/_api/images")) as { images?: string[] };
    setImages((j.images || []).map((s) => ({ name: s })));
    dataMark("images");
  }, []);

  const markRow = useCallback((project: string, kind: MarkKind, ms: number) => {
    setMarks((m) => ({ ...m, [project]: { kind, until: Date.now() + ms } }));
  }, []);

  const projOp = useCallback(async (name: string, op: "start" | "stop" | "rm") => {
    await api("POST", `/_api/${op}`, { project: name });
    dataInvalidate("projects");
    await refreshProjects(true);
  }, [refreshProjects]);

  const rollback = useCallback(async (name: string, v: string) => {
    await api("POST", "/_api/rollback", { project: name, version: v });
    dataInvalidate("projects");
    await refreshProjects(true);
  }, [refreshProjects]);

  const publishDone = useCallback(async (_name: string) => {
    dataInvalidate("projects", "images", "space");
    await Promise.allSettled([refreshProjects(true), ensureImages(true), refreshSpace(true)]);
  }, [refreshProjects, ensureImages, refreshSpace]);

  const rmImage = useCallback(async (name: string) => {
    await api("POST", "/_api/image-rm", { image: name });
    dataInvalidate("images", "space");
    await Promise.allSettled([ensureImages(true), refreshSpace(true)]);
  }, [ensureImages, refreshSpace]);

  // 桥填充（每渲染同步最新函数；会话层的登录并发/登出清理经此触发）
  useEffect(() => {
    dataBridge.refreshProjects = refreshProjects;
    dataBridge.ensureImages = ensureImages;
    dataBridge.refreshSpace = refreshSpace;
    dataBridge.reset = () => { setProjects([]); setImages([]); setMarks({}); };
  });

  // 30s 恒实时（plan-17 §3）：按当前标签只刷新对应数据源（boot 响应式：登录即启、登出即停）
  useEffect(() => {
    if (boot !== "app") return;
    const id = window.setInterval(() => {
      switch (tab) {
        case "proj": void refreshProjects(true).catch(() => {}); break;
        case "img": void ensureImages(true).catch(() => {}); void refreshSpace().catch(() => {}); break;
        case "db": void refreshSpace().catch(() => {}); break;
      }
    }, 30000);
    return () => clearInterval(id);
  }, [boot, tab, refreshProjects, ensureImages, refreshSpace]);

  // 断网恢复（§3/验收 8）：连网即强制重取项目与空间
  useEffect(() => {
    const onOnline = () => {
      if (!state.loggedIn) return;
      void refreshProjects(true).catch(() => {});
      void refreshSpace(true).catch(() => {});
    };
    window.addEventListener("online", onOnline);
    return () => window.removeEventListener("online", onOnline);
  }, [refreshProjects, refreshSpace]);

  const value = useMemo<DataState>(() => ({
    tab, setTab, pubPrefill, setPubPrefill,
    projects, refreshProjects, projOp, rollback, publishDone,
    images, ensureImages, rmImage, space, refreshSpace, marks, markRow,
  }), [tab, pubPrefill, projects, refreshProjects, projOp, rollback, publishDone,
    images, ensureImages, rmImage, space, refreshSpace, marks, markRow]);
  return <DataCtx.Provider value={value}>{children}</DataCtx.Provider>;
}

// —— 事件层：订阅/重连/可见性与登录态联动；事件 2s 去抖驱动项目列表刷新 ——
function EventsProvider({ children }: { children: ReactNode }) {
  const [events, setEvents] = useState<string[]>([]);
  const [evConnected, setEvConnected] = useState(false);
  const evAc = useRef<AbortController | null>(null);
  const tab = useContext(DataCtx)!.tab;
  const refreshProjects = useContext(DataCtx)!.refreshProjects;
  const boot = useContext(SessionCtx)!.boot;

  useEffect(() => {
    if (boot !== "app") {
      // 登出/改密收口：退订并清面板（跨账号不残留）
      evAc.current?.abort();
      evAc.current = null;
      setEvents([]);
      setEvConnected(false);
      return;
    }
    const want = !document.hidden && (tab === "proj" || tab === "admin");
    if (!want) {
      evAc.current?.abort();
      evAc.current = null;
      setEvConnected(false);
      return;
    }
    if (evAc.current) return; // 已订阅
    let alive = true;
    let evDebounce: ReturnType<typeof setTimeout> | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    const start = () => {
      if (!alive || evAc.current) return;
      const ac = new AbortController();
      evAc.current = ac;
      setEvConnected(true);
      consumeNdjson("/_api/events", (j) => {
        // §10.4：心跳/非平台动作不入面板；平台动作（sr-*）驱动面板与列表刷新
        const actor = typeof j.actor === "string" ? j.actor : "";
        if (j.hb === true) return;
        if (actor && !actor.startsWith("sr-")) return;
        setEvents((evs) => [...evs.slice(-19), new Date().toLocaleTimeString("sv-SE") + "  " + JSON.stringify(j)]);
        // 恒实时（§3）：事件 2s 去抖 → 项目列表即时收敛（容器启停/发布完成等）
        if (evDebounce) clearTimeout(evDebounce);
        evDebounce = setTimeout(() => { void refreshProjects(true).catch(() => {}); }, 2000);
      }, ac.signal).catch(() => {
        if (evAc.current === ac) { evAc.current = null; setEvConnected(false); }
        if (alive) retryTimer = setTimeout(start, 5000); // §3 断流 5s 重订
      });
    };
    start();
    const onVis = () => {
      if (document.hidden) {
        evAc.current?.abort();
        evAc.current = null;
        setEvConnected(false);
      } else {
        start(); // 回开 proj/admin 自动重订
      }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => {
      alive = false;
      document.removeEventListener("visibilitychange", onVis);
      if (retryTimer) clearTimeout(retryTimer);
      evAc.current?.abort();
      evAc.current = null;
    };
  }, [boot, tab, refreshProjects]);

  const value = useMemo<EventsState>(() => ({ events, evConnected }), [events, evConnected]);
  return <EventsCtx.Provider value={value}>{children}</EventsCtx.Provider>;
}

/** 聚合 hook：页面统一入口（三层 context 合并成既有 AppState 形状）。 */
/** 分页页码列表：≤7 全显；否则首尾+当前页邻近，中间省略（预览/结果分页共用）。 */
export function pageList(cur: number, total: number): (number | "…")[] {
  if (total <= 7) return Array.from({ length: total }, (_, i) => i + 1);
  const keep = [...new Set([1, 2, cur - 1, cur, cur + 1, total - 1, total].filter((x) => x >= 1 && x <= total))].sort((x, y) => x - y);
  const out: (number | "…")[] = [];
  let prev = 0;
  for (const n of keep) {
    if (n - prev > 1) out.push("…");
    out.push(n);
    prev = n;
  }
  return out;
}

export function useApp(): AppState {
  const ss = useContext(SessionCtx);
  const ds = useContext(DataCtx);
  const es = useContext(EventsCtx);
  if (!ss || !ds || !es) throw new Error("useApp outside AppProvider");
  return { ...ss, ...ds, ...es };
}

// —— 对外 URL 展示策略（plan-17 §6，对齐原版 proj.mjs）：① sr.publicBase → ② location.origin
//    （公网访问即公网地址）→ ③ 内网 origin 兜底 + 列表上方提示条 ——
const PRIV_HOST_RE = /^(localhost|127\.|0\.0\.0\.0|::1|\[::1\]|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)/i;

export function publicBase(saved: string): string {
  return saved.trim().replace(/\/+$/, "") || window.location.origin;
}
export function isPrivateHost(): boolean {
  return PRIV_HOST_RE.test(window.location.hostname);
}
export function externalUrl(saved: string, project: string): string {
  return `${publicBase(saved)}/${project}/`;
}

/** 原版 apiFail 语义：E_QUOTA 且磁盘空间不足 → 追加「到镜像库删除旧镜像」引导并刷新空间。 */
export function apiFailToast(e: unknown, what: string, refreshSpace?: (f?: boolean) => Promise<void>) {
  const err = e as { error?: string; msg?: string };
  if (err?.error === "E_QUOTA" && (err.msg || "").includes("磁盘空间不足")) {
    void refreshSpace?.(true);
    toast.error(`${what} 失败：${err.msg}；可到镜像库删除旧镜像后重试`);
    return;
  }
  toast.error(`${what} 失败：${err?.msg || err?.error || "网络异常或服务不可用"}`);
}


export { toast };