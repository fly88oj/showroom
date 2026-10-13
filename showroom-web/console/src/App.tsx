import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  NavigationMenu, NavigationMenuItem, NavigationMenuLink, NavigationMenuList,
} from "@/components/ui/navigation-menu";
import { Tabs, TabsContent } from "@/components/ui/tabs";
import { TooltipProvider } from "@/components/ui/tooltip";
import { SettingsDialog, PasswordDialog } from "@/components/SettingsDialog";
import { Login } from "@/pages/Login";
import { Projects } from "@/pages/Projects";
import { Images } from "@/pages/Images";
// 低频页懒加载（首包只含登录/壳/项目列表/镜像库）：发布/数据/帮助/管理按需分包
const Publish = lazy(() => import("@/pages/Publish").then((m) => ({ default: m.Publish })));
const Data = lazy(() => import("@/pages/Data").then((m) => ({ default: m.Data })));
const Help = lazy(() => import("@/pages/Help").then((m) => ({ default: m.Help })));
const Admin = lazy(() => import("@/pages/Admin").then((m) => ({ default: m.Admin })));
import { AppProvider, useApp, type Tab } from "@/lib/app";
import { Loader2Icon } from "lucide-react";

const PageFallback = () => (
  <div className="flex items-center justify-center gap-2 py-10 text-xs text-muted-foreground">
    <Loader2Icon className="size-4 animate-spin" />加载中…
  </div>
);
import { KeyRoundIcon, LogOutIcon, SettingsIcon } from "lucide-react";

/** 壳：登录门 → 顶栏（品牌/账号徽标菜单）+ NavigationMenu 主导航 + 1200px 居中内容卡（三层层级对齐原版） */
export function Shell() {
  const { boot, session, logout, tab, setTab, pubPrefill, setPubPrefill, projects } = useApp();
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [pwOpen, setPwOpen] = useState(false);
  const [base, setBase] = useState(() => localStorage.getItem("sr.publicBase") || ""); // 对外访问域名（sr.publicBase）：空且 origin 为私网 → 项目列表出内网提示条

  if (boot === "loading") {
    return (
      <div className="flex min-h-svh items-center justify-center gap-2 text-sm text-muted-foreground">
        <Loader2Icon className="size-4 animate-spin" />正在初始化登录组件…
      </div>
    );
  }
  if (boot === "login" || !session) return <Login />;

  const tabs: { v: Tab; label: string }[] = [
    { v: "proj", label: "我的项目" },
    { v: "pub", label: "发布" },
    { v: "img", label: "镜像库" },
    { v: "db", label: "数据" },
    { v: "help", label: "帮助" },
    ...(session.admin ? [{ v: "admin" as Tab, label: "管理" }] : []),
  ];

  return (
    <div className="min-h-svh pb-12">
      <FocusGuide projectsLen={projects.length} pubName="发布" />
      <header className="sticky top-0 z-30 border-b bg-background">
        <div className="mx-auto flex min-h-14 w-full max-w-[1200px] flex-wrap items-center justify-between gap-3 py-2">
          <div className="flex min-w-0 flex-wrap items-center gap-3">
            <h1 className="text-base font-bold">Showroom 控制台</h1>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="sm" className="gap-2 px-2">
                <Badge variant="secondary" className="font-mono">
                  {session.user}
                  {session.admin && (
                    <Badge variant="warning" className="ml-1 h-4 px-1 text-[10px]">管</Badge>
                  )}
                </Badge>
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={() => setSettingsOpen(true)}>
                <SettingsIcon data-icon="inline-start" />设置…
              </DropdownMenuItem>
              <DropdownMenuItem onSelect={() => setPwOpen(true)}>
                <KeyRoundIcon data-icon="inline-start" />修改密码…
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem variant="destructive" onSelect={logout}>
                <LogOutIcon data-icon="inline-start" />退出登录
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </header>

      <Tabs value={tab} onValueChange={(v) => setTab(v as Tab)}>
        <div className="sticky top-14 z-20 border-b bg-background">
          <div className="mx-auto w-full max-w-[1200px] py-1.5">
            <NavigationMenu viewport={false} className="max-w-none flex-1 justify-start">
              <NavigationMenuList className="flex-none justify-start gap-1">
                {tabs.map((t) => (
                  <NavigationMenuItem key={t.v}>
                    <NavigationMenuLink
                      active={tab === t.v}
                      onSelect={() => setTab(t.v)}
                      className="h-9 px-3 font-medium data-active:bg-primary data-active:text-primary-foreground data-active:hover:bg-primary"
                    >
                      {t.label}
                    </NavigationMenuLink>
                  </NavigationMenuItem>
                ))}
              </NavigationMenuList>
            </NavigationMenu>
          </div>
        </div>

        <div className="mx-auto mt-3 w-full max-w-[1200px]">
          <Card>
            <CardContent>
              <TabsContent value="proj" forceMount className="data-[state=inactive]:hidden">
                <Projects base={base} onOpenSettings={() => setSettingsOpen(true)} />
              </TabsContent>
              <TabsContent value="pub" forceMount className="data-[state=inactive]:hidden">
                <Suspense fallback={<PageFallback />}><Publish base={base} prefill={pubPrefill} onConsumed={() => setPubPrefill(null)} onOpenSettings={() => setSettingsOpen(true)} /></Suspense>
              </TabsContent>
              <TabsContent value="img" forceMount className="data-[state=inactive]:hidden">
                <Images />
              </TabsContent>
              <TabsContent value="db" forceMount className="data-[state=inactive]:hidden">
                <Suspense fallback={<PageFallback />}><Data onOpenSettings={() => setSettingsOpen(true)} /></Suspense>
              </TabsContent>
              <TabsContent value="help" forceMount className="data-[state=inactive]:hidden">
                <Suspense fallback={<PageFallback />}><Help /></Suspense>
              </TabsContent>
              {session.admin && (
                <TabsContent value="admin" forceMount className="data-[state=inactive]:hidden">
                  <Suspense fallback={<PageFallback />}><Admin /></Suspense>
                </TabsContent>
              )}
            </CardContent>
          </Card>
        </div>
      </Tabs>

      <SettingsDialog open={settingsOpen} onOpenChange={setSettingsOpen} base={base} onBaseChange={setBase} />
      <PasswordDialog open={pwOpen} onOpenChange={setPwOpen} />
    </div>
  );
}

/** 登录焦点引导（验收 16，一次性）：已有项目 → URL 列首链接；空账号 → 「发布」导航。 */
function FocusGuide({ projectsLen, pubName }: { projectsLen: number; pubName: string }) {
  const done = useRef(false);
  useEffect(() => {
    if (done.current) return;
    const id = setTimeout(() => { // 4 路并发拉数完成后
      done.current = true;
      const first = document.querySelector<HTMLAnchorElement>("table a[target=_blank]");
      if (projectsLen && first) { first.focus(); return; }
      const links = [...document.querySelectorAll<HTMLAnchorElement>("nav a")];
      links.find((a) => a.textContent?.trim() === pubName)?.focus();
    }, 2500);
    return () => clearTimeout(id);
  }, [projectsLen, pubName]);
  return null;
}

export default function App() {
  return (
    <AppProvider>
      <TooltipProvider delayDuration={200}>
        <Shell />
      </TooltipProvider>
    </AppProvider>
  );
}
