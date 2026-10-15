import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from "@/components/ui/table";
import { DownloadIcon } from "lucide-react";
import { toast } from "sonner";
import { apiDo, respErr } from "@/lib/api";
import { CLI_TABLE, ERROR_TABLE, TOOLS_TABLE } from "@/lib/help";
import { ENV_DESC } from "@/lib/state";
import { useApp } from "@/lib/app";

/** 帮助：账号 / CLI / 镜像规范 / 工具下载 / 错误码（子导航小一档，对齐原版 help-subnav） */
export function Help() {
  const { session } = useApp();
  const admin = session?.admin;

  return (
    <div className="flex flex-col gap-4">
      <Tabs defaultValue="account" className="flex flex-col gap-4">
        <TabsList variant="line" className="h-9 w-fit gap-0.5 overflow-x-auto flex-nowrap max-w-full">
          <TabsTrigger value="account">账号</TabsTrigger>
          <TabsTrigger value="cli">CLI 使用</TabsTrigger>
          <TabsTrigger value="mirror">镜像规范</TabsTrigger>
          <TabsTrigger value="mw">中间件连接</TabsTrigger>
          <TabsTrigger value="tools">工具下载</TabsTrigger>
          <TabsTrigger value="errors">错误码</TabsTrigger>
        </TabsList>

        <TabsContent value="account" className="mt-4 flex flex-col gap-2">
          <h2 className="text-sm font-semibold text-info">登录与账号</h2>
          <ul className="list-disc pl-5 text-xs leading-6 text-muted-foreground">
            <li>账号与登录密码由管理员发放。</li>
            <li>忘记密码请联系管理员重置（控制台无法自助找回）。</li>
          </ul>
        </TabsContent>

        <TabsContent value="cli" className="mt-4 flex flex-col gap-4">
          <h2 className="text-sm font-semibold text-info">CLI 使用</h2>
          <ul className="list-disc pl-5 text-xs leading-6 text-muted-foreground">
            <li>
              CLI 与控制台同一模型（统一账号密码认证）：
              <span className="font-mono break-all">showroom login &lt;账号&gt;</span>
              输一次密码即自动配置本机（含镜像库凭据），此后全部子命令免密码（gh/ssh 模型）；
              <span className="font-mono break-all">showroom logout</span> 退出（清本机并远程吊销钥匙）。
            </li>
            <li>无账号 / 忘记密码：联系管理员发放或重置（<span className="font-mono break-all">showroomctl user-add &lt;名&gt; --gen-password</span>）。</li>
            <li>镜像上传与备份加密需要平台公钥：登录控制台，在本页「工具下载」下载 <span className="font-mono break-all">platform_rsa.pub</span>，放入 <span className="font-mono break-all">~/.showroom/platform_rsa.pub</span>（一次性）。</li>
          </ul>
          <h2 className="text-sm font-semibold text-info">CLI 命令对照</h2>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow className="bg-muted/30"><TableHead className="w-56">控制台功能</TableHead><TableHead>CLI 等价命令</TableHead></TableRow>
              </TableHeader>
              <TableBody>
                {CLI_TABLE.map((r) => (
                  (r.adminOnly && !admin)
                    ? null
                    : (
                      <TableRow key={r.web}>
                        <TableCell className="text-xs">
                          {r.web}
                          {r.adminOnly && <Badge variant="warning" className="ml-2">管理员</Badge>}
                        </TableCell>
                        <TableCell className="font-mono text-xs">{r.cli}</TableCell>
                      </TableRow>
                    )
                ))}
              </TableBody>
            </Table>
          </div>
        </TabsContent>

        <TabsContent value="mirror" className="mt-4 flex flex-col gap-4">
          <h2 className="text-sm font-semibold text-info">镜像规范</h2>
          <ul className="list-disc pl-5 text-xs leading-6 text-muted-foreground">
            <li>项目须监听容器内 <span className="font-mono break-all">8080</span> 端口（其他端口在发布表单的端口字段如实填写）。</li>
            <li>项目经子路径对外发布：访问地址恒为 <span className="font-mono break-all">/&lt;project&gt;/</span>，读 <span className="font-mono break-all">SHOWROOM_BASE_PATH</span>（值即该前缀）适配，或页面内一律使用相对 URL。</li>
            <li>建议在 Dockerfile 里配置 <span className="font-mono break-all">HEALTHCHECK</span>，平台据此判定健康并自愈。</li>
            <li>镜像内配置文件替换推荐 <span className="font-mono break-all">envsubst &lt; config.tpl &gt; config</span> 模式（占位符由注入的环境变量填充）。</li>
          </ul>
          <h2 className="text-sm font-semibold text-info">环境变量清单（发布时自动注入）</h2>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow className="bg-muted/30"><TableHead className="w-56">键</TableHead><TableHead>用途</TableHead></TableRow>
              </TableHeader>
              <TableBody>
                {Object.entries(ENV_DESC).map(([k, d]) => (
                  <TableRow key={k}>
                    <TableCell className="font-mono text-xs">{k}</TableCell>
                    <TableCell className="text-xs text-muted-foreground">{d}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          <p className="text-xs text-muted-foreground">
            当前值可随时在「我的项目 → 更多 → 环境变量」里查看（<span className="font-mono break-all">showroom env</span> 同）。
          </p>
        </TabsContent>

        <TabsContent value="tools" className="mt-4 flex flex-col gap-3">
          <h2 className="text-sm font-semibold text-info">工具下载</h2>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow className="bg-muted/30"><TableHead className="w-52">文件</TableHead><TableHead>说明</TableHead><TableHead className="w-20 text-right">大小</TableHead><TableHead className="w-24 text-right">操作</TableHead></TableRow>
              </TableHeader>
              <TableBody>
                {TOOLS_TABLE.map((t) => (
                  <TableRow key={t.file}>
                    <TableCell className="font-mono text-xs">{t.file}</TableCell>
                    <TableCell className="max-w-md text-xs text-muted-foreground">{t.desc}</TableCell>
                    <TableCell data-size className="text-right font-mono text-xs text-muted-foreground">—</TableCell>
                    <TableCell className="text-right">
                      <Button
                        size="sm" variant="outline"
                        onClick={async (ev) => {
                          const btn = ev.currentTarget;
                          btn.disabled = true;
                          try {
                            const resp = await apiDo("GET", `/_api/tool/${encodeURIComponent(t.file)}`, null);
                            if (!resp.ok) throw await respErr(resp);
                            const blob = await resp.blob();
                            const a = document.createElement("a");
                            a.href = URL.createObjectURL(blob);
                            a.download = t.file;
                            a.click();
                            setTimeout(() => URL.revokeObjectURL(a.href), 60000);
                            const mb = (blob.size / 1024 / 1024).toFixed(1);
                            const cell = btn.closest("tr")?.querySelector("[data-size]");
                            if (cell) cell.textContent = `${mb} MB`;
                            toast.success(`已下载 ${t.file}（${mb} MB）`);
                          } catch (e) {
                            toast.error(`下载 ${t.file} 失败：${(e as { msg?: string }).msg ?? ""}`);
                          } finally {
                            btn.disabled = false;
                          }
                        }}
                      >
                        <DownloadIcon data-icon="inline-start" />下载
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          <p className="text-xs text-muted-foreground">文件大小在首次下载后显示。CLI 首次配置见「CLI 使用」栏目。</p>
        </TabsContent>

        <TabsContent value="mw" className="mt-4 flex flex-col gap-3">
          <h2 className="text-sm font-semibold text-info">容器内如何连接中间件</h2>
          <p className="text-xs text-muted-foreground">发布时平台自动向每个容器注入以下环境变量（12-factor：应用读 env 即可，无需配置文件）；真实值随时在「我的项目 → 更多 → 环境变量」或 CLI <code className="font-mono break-all">showroom env &lt;项目&gt;</code> 查看。</p>
          <div className="rounded-lg border">
            <table className="w-full text-xs">
              <tbody>
                {[
                  ["DB_HOST / DB_PORT", ENV_DESC.DB_HOST],
                  ["DB_NAME / DB_USER", ENV_DESC.DB_NAME],
                  ["DB_PASSWORD", ENV_DESC.DB_PASSWORD],
                  ["DATABASE_URL", ENV_DESC.DATABASE_URL],
                  ["REDIS_HOST / REDIS_PORT / REDIS_DB", ENV_DESC.REDIS_HOST],
                  ["REDIS_URL", ENV_DESC.REDIS_URL],
                  ["SHOWROOM_PROJECT / SHOWROOM_BASE_PATH", ENV_DESC.SHOWROOM_BASE_PATH],
                  ["SHOWROOM_SVC_<名称>_HOST/_PORT", "多容器服务发现：全体容器可发现主服务（main）与各附加服务"],
                ].map(([k, d]) => (
                  <tr key={k} className="border-b last:border-0">
                    <td className="w-72 p-2 font-mono align-top">{k}</td>
                    <td className="p-2 text-muted-foreground align-top">{d}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <h3 className="text-sm font-semibold mt-2">多容器项目（附加服务）</h3>
          <p className="text-xs text-muted-foreground">发布页「部署参数 → 附加服务」或 CLI <code className="font-mono break-all">showroom publish … --svc api=my/api:3000,3000</code> 添加；附加服务仅内网（不出公网端口），与主服务同网络可按容器名互访；日志 <code className="font-mono break-all">showroom logs &lt;项目&gt; --svc api</code>。</p>
          <h3 className="text-sm font-semibold mt-2">自定义环境变量</h3>
          <p className="text-xs text-muted-foreground">两层：全体变量注入每个容器；附加服务可再设专属变量只注入该服务。应用自有键名任意设（如 LOG_LEVEL / TIMEZONE）；同键可覆写平台默认值。回滚=整体恢复该版本配置（主服务+附加服务+全部变量）。</p>
          <h3 className="text-sm font-semibold mt-2">自定义变量引用中间件凭据（${'{'}VAR{'}'} 插值）</h3>
          <p className="text-xs text-muted-foreground">自定义变量的值可引用平台注入的任意键——例如程序用自己的键名时：<code className="font-mono break-all">MYAPP_DB_URL=postgres://${'{'}DB_USER{'}'}:${'{'}DB_PASSWORD{'}'}@${'{'}DB_HOST{'}'}:${'{'}DB_PORT{'}'}/${'{'}DB_NAME{'}'}</code>，或直接 <code className="font-mono break-all">MYAPP_DB_URL=${'{'}DATABASE_URL{'}'}</code>。随机生成的密码无需知道，插值在发布时自动解析。附加服务容器同样持有全部 DB_*/REDIS_* 凭据（共用项目库），可直接连接 sr-postgres / sr-redis。</p>
          <h3 className="text-sm font-semibold mt-2">镜像内配置文件替换</h3>
          <p className="text-xs text-muted-foreground">推荐入口脚本 <code className="font-mono break-all">envsubst &lt; config.tpl &gt; config</code>——模板占位符（<code className="font-mono break-all">${'$'}{'{'}DB_PASSWORD{'}'}</code> 等）在容器启动时由注入的真实值替换。</p>
        </TabsContent>
        <TabsContent value="errors" className="mt-4 flex flex-col gap-3">
          <h2 className="text-sm font-semibold text-info">常见错误码</h2>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow className="bg-muted/30"><TableHead className="w-24">错误码</TableHead><TableHead>含义与处理</TableHead></TableRow>
              </TableHeader>
              <TableBody>
                {ERROR_TABLE.map((r) => (
                  <TableRow key={r.code}>
                    <TableCell className="font-mono text-xs">{r.code}</TableCell>
                    <TableCell className="max-w-xl text-xs text-muted-foreground">{r.desc}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
        </TabsContent>
      </Tabs>
    </div>
  );
}
