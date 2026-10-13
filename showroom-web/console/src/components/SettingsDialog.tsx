import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { Textarea } from "@/components/ui/textarea";
import { toast } from "sonner";
import { api } from "@/lib/api";
import { opaqueEnd, opaqueFinish, opaqueRegEnd, opaqueRegFinish, opaqueRegStart, opaqueStart, postAuthJson } from "@/lib/auth";
import { state } from "@/lib/state";

/** 设置：对外访问域名 + 平台公钥 PEM 收纳 + 修改密码入口（真链路） */
export function SettingsDialog({
  open, onOpenChange, base, onBaseChange,
}: {
  open: boolean; onOpenChange: (v: boolean) => void;
  base: string; onBaseChange: (v: string) => void;
}) {
  const [draft, setDraft] = useState(base);
  const [err, setErr] = useState("");
  const [pem, setPem] = useState(state.pem);
  const [pwOpen, setPwOpen] = useState(false);

  useEffect(() => { if (open) { setDraft(base); setPem(state.pem); setErr(""); } }, [open, base]);

  /** 原版行为：base 输入即存（无保存按钮）；校验错就地上呈。 */
  function onBaseInput(v: string) {
    setDraft(v);
    setErr(v && !/^https?:\/\/.+/.test(v) ? "需以 http:// 或 https:// 开头" : "");
    if (!v || /^https?:\/\/.+/.test(v)) {
      const cleaned = v.trim().replace(/\/+$/, "");
      localStorage.setItem("sr.publicBase", cleaned);
      onBaseChange(cleaned);
    }
  }

  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>设置</DialogTitle>
            <DialogDescription>
              对外访问域名用于项目列表与完成页主链接拼接；留空则用当前访问地址。
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <Field data-invalid={!!err}>
              <FieldLabel htmlFor="set-base">对外访问域名</FieldLabel>
              <Input
                id="set-base" className="font-mono" placeholder="http://…:…"
                value={draft} onChange={(e) => onBaseInput(e.target.value)} aria-invalid={!!err}
              />
              {err ? <FieldError>{err}</FieldError> : null}
            </Field>
            <Field>
              <FieldLabel htmlFor="set-pem">
                平台公钥 PEM（管理员分发的 <span className="font-mono">platform_rsa.pub</span>）
              </FieldLabel>
              <Textarea
                id="set-pem" className="min-h-32 font-mono text-xs" rows={7} value={pem}
                onChange={(e) => {
                  setPem(e.target.value);
                  state.pem = e.target.value; // PEM 事实源 state：输入即存（现版行为）
                  localStorage.setItem("sr.pempub", e.target.value);
                }}
              />
              <FieldDescription>
                镜像上传与数据库恢复的加密信封公钥。登录后通常已自动配置，无需粘贴，此处仅兜底。
              </FieldDescription>
            </Field>
          </FieldGroup>
          <Separator />
          <DialogFooter className="items-center">
            <Button variant="outline" onClick={() => setPwOpen(true)}>
              修改密码…
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <PasswordDialog open={pwOpen} onOpenChange={setPwOpen} />
    </>
  );
}

/** 修改密码（plan-16 §2.2 真序列）：旧密码本地校验（login-begin+finish，不产生登录态）
 *  → pw-begin {m1} → 本地 reg finish → pw-commit {record} → 引导重登。 */
function PasswordDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const [cur, setCur] = useState("");
  const [nw, setNw] = useState("");
  const [nw2, setNw2] = useState("");
  const [errs, setErrs] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const { logout } = { logout: () => window.location.reload() }; // 改密成功后由调用方登出（见下）

  async function submit() {
    const e: Record<string, string> = {};
    if (!cur) e.cur = "请输入当前密码；忘记密码请联系管理员重置";
    if (!nw) e.nw = "请输入新密码";
    else if (nw === cur) e.nw = "新密码与当前密码相同，请换一个";
    if (nw !== nw2) e.nw2 = "两次输入的新密码不一致，请重新输入";
    setErrs(e);
    if (Object.keys(e).length) return;
    setBusy(true);
    try {
      // ① 当前密码校验（完整 login 序列但不 login-finish：不产生登录态）
      const st = opaqueStart(state.user, cur);
      let ke2: string;
      try {
        ke2 = ((await postAuthJson("/_api/auth/login-begin", { user: state.user, ke1: st.ke1 })) as { ke2: string }).ke2;
      } catch (err) {
        opaqueEnd(st.sid);
        throw err;
      }
      try {
        opaqueFinish(st.sid, cur, ke2);
      } catch (err) {
        if ((err as { error?: string }).error === "E_PW_LOCAL") {
          throw { error: "E_PW_CUR", msg: "" };
        }
        throw err;
      } finally {
        opaqueEnd(st.sid);
      }
      // ② pw 三件
      const rs = opaqueRegStart(state.user, nw);
      let m2: string;
      try {
        m2 = ((await api("POST", "/_api/auth/pw-begin", { user: state.user, m1: rs.m1 })) as { m2: string }).m2;
      } catch (err) {
        opaqueRegEnd(rs.sid);
        throw err;
      }
      let record: string;
      try {
        record = opaqueRegFinish(rs.sid, nw, m2);
      } finally {
        opaqueRegEnd(rs.sid);
      }
      await api("POST", "/_api/auth/pw-commit", { user: state.user, record });
      setCur(""); setNw(""); setNw2("");
      onOpenChange(false);
      toast.success("密码修改成功：请使用新密码重新登录。");
      // 收口：结束当前会话，引导新密码重登（清本地钥匙）
      localStorage.removeItem("sr.seed");
      localStorage.removeItem("sr.user");
      setTimeout(() => window.location.reload(), 800);
    } catch (err2) {
      const e2 = err2 as { error?: string; msg?: string };
      if (e2.error === "E_PW_CUR") {
        setErrs({ cur: "当前密码不正确（或该账号密码已被重置），请重输" });
      } else {
        toast.error(`修改密码失败：${e2.msg || e2.error || "网络异常"}`);
      }
    } finally {
      setBusy(false);
    }
  }

  void logout;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>修改密码</DialogTitle>
          <DialogDescription>新旧密码均只在本机参与盲化，全程不明文上线。</DialogDescription>
        </DialogHeader>
        <FieldGroup>
          <Field data-invalid={!!errs.cur}>
            <FieldLabel htmlFor="pw-cur">当前密码</FieldLabel>
            <Input id="pw-cur" type="password" autoComplete="current-password"
              value={cur} onChange={(e) => setCur(e.target.value)} aria-invalid={!!errs.cur} />
            {errs.cur ? <FieldError>{errs.cur}</FieldError> : null}
          </Field>
          <Field data-invalid={!!errs.nw}>
            <FieldLabel htmlFor="pw-new">新密码</FieldLabel>
            <Input id="pw-new" type="password" autoComplete="new-password"
              value={nw} onChange={(e) => setNw(e.target.value)} aria-invalid={!!errs.nw} />
            {errs.nw ? <FieldError>{errs.nw}</FieldError> : null}
          </Field>
          <Field data-invalid={!!errs.nw2}>
            <FieldLabel htmlFor="pw-new2">确认新密码</FieldLabel>
            <Input id="pw-new2" type="password" autoComplete="new-password"
              value={nw2} onChange={(e) => setNw2(e.target.value)} aria-invalid={!!errs.nw2} />
            {errs.nw2 ? <FieldError>{errs.nw2}</FieldError> : null}
          </Field>
          <div className="flex items-center gap-3">
            <Button onClick={submit} disabled={busy}>{busy ? "修改中…" : "确认修改"}</Button>
          </div>
        </FieldGroup>
      </DialogContent>
    </Dialog>
  );
}
