// —— app.tsx：迁移过渡数据层（阶段一）——
// 会话/项目/镜像/空间走真实 /_api（协议与 TTL 纪律同 dist/js）；发布上传链路、DB、管理端、
// 事件流为阶段二目标，本层保留接口形状让全站可构建可部署。
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
export interface SpaceInfo { availGb: number; totalGb: number; reserveGb: number }

export interface Session { user: string; admin: boolean }

interface AppState {
  boot: "loading" | "login" | "app"; // wasm 装载中 / 登录窗 / 控制台
  session: Session | null;
  login: (user: string, pass: string) => Promise<void>;
  logout: () => void;
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
  events: string[];
  evConnected: boolean;
  /** 行内在途标记（§4）：发布中…/上传中/重建中(60s)/异常(60s)——服务端 status 收敛前的本地过渡 */
  marks: Record<string, { kind: "publishing" | "uploading" | "rebuilding" | "failed"; until: number }>;
  markRow: (project: string, kind: "publishing" | "uploading" | "rebuilding" | "failed", ms: number) => void;
  /** 改密成功收口：页内登出（清本机钥匙）+ 登录框提示行（原版 shell.mjs 语义，不 reload） */
  onPwChanged: () => void;
  /** 登录框一次性提示（改密成功后由 onPwChanged 写入） */
  loginNotice: string;
}

const Ctx = createContext<AppState | null>(null);

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
  const [boot, setBoot] = useState<"loading" | "login" | "app">("loading");
  const [session, setSession] = useState<Session | null>(null);
  const [loginNotice, setLoginNotice] = useState("");
  const [tab, setTabRaw] = useState<Tab>("proj");
  const [pubPrefill, setPubPrefill] = useState<string | null>(null);
  const [projects, setProjects] = useState<Project[]>([]);
  const [images, setImages] = useState<ImageItem[]>([]);
  const [space, setSpace] = useState<SpaceInfo>({ availGb: 0, totalGb: 0, reserveGb: 0 });
  const [events, setEvents] = useState<string[]>([]);
  const [marks, setMarks] = useState<Record<string, { kind: "publishing" | "uploading" | "rebuilding" | "failed"; until: number }>>({});
  const [evConnected, setEvConnected] = useState(false);
  const evAc = useRef<AbortController | null>(null);

  const setTab = useCallback((t: Tab) => {
    setTabRaw(t);
    if (t !== "pub") setPubPrefill(null);
  }, []);

  // —— 数据面（TTL 纪律：dataFresh/dataMark；写操作 invalidate 后强制重取）——
  const refreshProjects = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("projects"))) return;
    const j = (await api("GET", "/_api/projects")) as { projects?: Record<string, unknown>[] };
    const list = (j.projects || []).map(mapProject);
    state.projects = list as never;
    setProjects(list);
    dataMark("projects");
  }, []);

  const refreshSpace = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("space"))) return;
    const j = (await api("GET", "/_api/space")) as { available_mb?: number; reserve_mb?: number };
    const avail = (Number(j.available_mb) - Number(j.reserve_mb)) / 1024; // 显示口径＝可用−预留（§14.1）
    if (Number.isFinite(avail)) {
      setSpace({ availGb: +(avail).toFixed(1), totalGb: 0, reserveGb: +(Number(j.reserve_mb) / 1024).toFixed(0) });
      dataMark("space");
    }
  }, []);

  const ensureImages = useCallback(async (force = false) => {
    if (!state.loggedIn || (!force && dataFresh("images"))) return;
    const j = (await api("GET", "/_api/images")) as { images?: string[] };
    setImages((j.images || []).map((s) => ({ name: s })));
    dataMark("images");
  }, []);

  const syncAdmin = useCallback(async () => {
    const j = (await api("GET", "/_api/whoami")) as { admin?: boolean; platform_pem?: string };
    state.admin = !!j.admin;
    if (j.platform_pem && !state.pem) hydratePem(j.platform_pem); // whoami 自动水合（守卫：不覆盖已有）
    setSession((s) => (s ? { ...s, admin: !!j.admin } : s));
  }, []);

  const enterApp = useCallback(async () => {
    state.loggedIn = true;
    setBoot("app");
    setSession({ user: state.user, admin: state.admin });
    startHeartbeat(() => { // E_AUTH：钥匙过期/吊销 → 踢回登录窗
      state.loggedIn = false;
      setBoot("login");
      setSession(null);
    });
    // plan-17 §3：登录即 4 路并发（projects/images/space/whoami），任一路失败不阻断
    await Promise.allSettled([refreshProjects(true), ensureImages(true), refreshSpace(true), syncAdmin()]);
  }, [refreshProjects, ensureImages, refreshSpace, syncAdmin]);

  const login = useCallback(async (user: string, pass: string) => {
    setLoginNotice("");
    await passwordLogin(user.trim(), pass);
    await enterApp();
  }, [enterApp]);

  const logout = useCallback(() => {
    stopHeartbeat(); // 登出即停心跳（防清钥后的无效请求）
    if (state.seed && state.user && state.kid) {
      api("POST", "/_api/key-selfrm", { kid: state.kid }).catch(() => {});
    }
    state.loggedIn = false;
    state.admin = false;
    state.boundAdmin = false;
    localStorage.removeItem("sr.seed");
    localStorage.removeItem("sr.user");
    setProjects([]); setImages([]); setEvents([]); setEvConnected(false);
    setSession(null);
    setBoot("login");
  }, []);

  const onPwChanged = useCallback(() => {
    // 原版 B2 语义：改密=结束当前会话（清本机钥匙）+ 登录框提示行；不刷新页面
    state.loggedIn = false;
    localStorage.removeItem("sr.seed");
    localStorage.removeItem("sr.user");
    setLoginNotice("密码已修改：请输入新密码重新登录。");
    setSession(null);
    setBoot("login");
  }, []);

  const markRow = useCallback((project: string, kind: "publishing" | "uploading" | "rebuilding" | "failed", ms: number) => {
    setMarks((m) => ({ ...m, [project]: { kind, until: Date.now() + ms } }));
  }, []);

  const projOp = useCallback(async (name: string, op: "start" | "stop" | "rm") => {
    await api("POST", `/_api/${op}`, { project: name });
    dataInvalidate("projects");
    await refreshProjects(true); // 写操作强制失效
  }, [refreshProjects]);

  const rollback = useCallback(async (name: string, v: string) => {
    await api("POST", "/_api/rollback", { project: name, version: v });
    dataInvalidate("projects");
    await refreshProjects(true);
  }, [refreshProjects]);

  const publishDone = useCallback(async (_name: string) => {
    // 阶段二接完整发布链路（seal 分片上传 + publish --wait）；此处先同步列表
    dataInvalidate("projects", "images", "space");
    await Promise.allSettled([refreshProjects(true), ensureImages(true), refreshSpace(true)]);
  }, [refreshProjects, ensureImages, refreshSpace]);

  const rmImage = useCallback(async (name: string) => {
    await api("POST", "/_api/image-rm", { image: name });
    dataInvalidate("images", "space");
    await Promise.allSettled([ensureImages(true), refreshSpace(true)]);
  }, [ensureImages, refreshSpace]);

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
          state.user = localStorage.getItem("sr.user") || ""; // 保留账号名便于重登
        }
      }
      setBoot("login");
    })().catch(() => setBoot("login"));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 事件流（§3）：登录即订阅；页签隐藏退订；E_AUTH 由心跳统一踢回
  useEffect(() => {
    if (boot !== "app") return;
    const want = !document.hidden && (tab === "proj" || tab === "admin");
    if (!want) {
      evAc.current?.abort();
      evAc.current = null;
      setEvConnected(false);
      return;
    }
    if (evAc.current) return; // 已订阅
    const ac = new AbortController();
    evAc.current = ac;
    setEvConnected(true);
    let evDebounce: ReturnType<typeof setTimeout> | null = null;
    consumeNdjson("/_api/events", (j) => {
      setEvents((evs) => [...evs.slice(-19), new Date().toLocaleTimeString("sv-SE") + "  " + JSON.stringify(j)]);
      // 恒实时（§3）：事件 2s 去抖 → 项目列表即时收敛（容器启停/发布完成等）
      if (evDebounce) clearTimeout(evDebounce);
      evDebounce = setTimeout(() => { void refreshProjects(true); }, 2000);
    }, ac.signal).catch(() => {
      if (evAc.current === ac) { evAc.current = null; setEvConnected(false); }
    });
    const onVis = () => {
      if (document.hidden) { ac.abort(); if (evAc.current === ac) evAc.current = null; setEvConnected(false); }
    };
    document.addEventListener("visibilitychange", onVis);
    return () => {
      document.removeEventListener("visibilitychange", onVis);
      ac.abort();
      if (evAc.current === ac) evAc.current = null;
    };
  }, [boot, tab, refreshProjects]);

  // 断网恢复（§3/验收 8）：连网即强制重取项目与空间（免刷新自动补齐）
  useEffect(() => {
    const onOnline = () => {
      if (!state.loggedIn) return;
      void refreshProjects(true);
      void refreshSpace(true);
    };
    window.addEventListener("online", onOnline);
    return () => window.removeEventListener("online", onOnline);
  }, [refreshProjects, refreshSpace]);

  // 30s 恒实时（plan-17 §3）：按当前标签只刷新对应数据源
  useEffect(() => {
    if (boot !== "app") return;
    const id = window.setInterval(() => {
      switch (tab) {
        case "proj": void refreshProjects(true); break;
        case "img": void ensureImages(true); void refreshSpace(); break;
        case "db": void refreshSpace(); break;
      }
    }, 30000);
    return () => clearInterval(id);
  }, [boot, tab, refreshProjects, ensureImages, refreshSpace]);

  const value = useMemo<AppState>(() => ({
    boot, session, login, logout, tab, setTab, pubPrefill, setPubPrefill,
    projects, refreshProjects, projOp, rollback, publishDone,
    images, ensureImages, rmImage, space, refreshSpace,
    events, evConnected, marks, markRow, onPwChanged, loginNotice,
  }), [boot, session, login, logout, tab, pubPrefill, projects, refreshProjects, loginNotice,
    projOp, rollback, publishDone, images, ensureImages, rmImage, space, refreshSpace, events, evConnected]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
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

export function useApp(): AppState {
  const v = useContext(Ctx);
  if (!v) throw new Error("useApp outside provider");
  return v;
}

export { toast };