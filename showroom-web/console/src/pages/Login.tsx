import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Card, CardContent, CardHeader, CardTitle,
} from "@/components/ui/card";
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { useApp } from "@/lib/app";

/** 登录页：一步「账号+密码」（原版 plan-16 §3）；Demo 免密一键进入两种角色 */
export function Login() {
  const { login } = useApp();
  const [user, setUser] = useState("");
  const [pass, setPass] = useState("");
  const [errU, setErrU] = useState("");
  const [errP, setErrP] = useState("");
  const [fb, setFb] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErrU(""); setErrP(""); setFb("");
    if (!user.trim()) { setErrU("请输入账号（发布者账号，由管理员发放）"); return; }
    if (!pass) { setErrP("请输入密码；忘记密码请联系管理员重置"); return; }
    setBusy(true);
    try {
      await login(user.trim(), pass);
      // 成功静默（R9 铁律）：不弹横幅直接进入；密码用后即清（不留 DOM）
      setPass("");
    } catch (e) {
      const err = e as { error?: string; msg?: string };
      if (err?.error === "E_PW_LOCAL") {
        // 客户端无法也不应区分「密码错」与「未设密码」（防枚举的一致代价，§3）
        setErrP("密码错误，或该账号尚未设置密码（请联系管理员重置）");
      } else if (err?.error === "E_AUTH") {
        setErrP("登录被服务端拒绝（可能为限流或服务暂不可用），请稍后重试；忘记密码请联系管理员重置");
      } else {
        setErrP(`登录失败（${err?.msg || "网络异常或服务不可用"}），请稍后重试`);
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mx-auto max-w-xl px-4 pt-16">
      <Card>
        <CardHeader>
          <CardTitle className="text-lg">Showroom 控制台</CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} noValidate>
            <FieldGroup>
              <Field data-invalid={!!errU}>
                <FieldLabel htmlFor="lg-user">账号（发布者账号）</FieldLabel>
                <Input
                  id="lg-user" placeholder="如 alice…" autoComplete="username"
                  value={user} onChange={(e) => setUser(e.target.value)} aria-invalid={!!errU}
                />
                {errU ? <FieldError>{errU}</FieldError> : null}
              </Field>
              <Field data-invalid={!!errP}>
                <FieldLabel htmlFor="lg-pass">密码（非 docker push 用的镜像库密码）</FieldLabel>
                <Input
                  id="lg-pass" type="password" placeholder="输入账号密码…"
                  autoComplete="current-password" value={pass}
                  onChange={(e) => setPass(e.target.value)} aria-invalid={!!errP}
                />
                {errP ? <FieldError>{errP}</FieldError> : null}
              </Field>
              <div className="flex items-center gap-3">
                <Button type="submit" disabled={busy}>{busy ? "登录中…" : "登录"}</Button>
                <span aria-live="polite" className="text-sm text-bad">{fb}</span>
              </div>
            </FieldGroup>
          </form>
          <p className="mt-3 text-xs text-muted-foreground">忘记密码请联系管理员重置。</p>
        </CardContent>
      </Card>
    </div>
  );
}
