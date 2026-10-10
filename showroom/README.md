# showroom — Showroom 发布者 CLI

单二进制发布工具：推送镜像、发布/回滚展项、查看日志与数据库/Redis 连接信息。

## 安装

单二进制，除 `docker` 命令行（仅 `push` 需要）外无运行时依赖：

```sh
cargo build --release
cp target/release/showroom ~/.local/bin/   # 或任意 PATH 目录
```

## 快速上手

```sh
showroom keygen                                            # 生成密钥，把打印的公钥串发给管理员登记
showroom config --user alice --server-lan http://<内网地址> \
    --server-public http://<公网地址:端口> \
    --registry-user alice --registry-password <registry密码>  # 写 ~/.showroom/config.toml（0600）
showroom push demo v1                                      # 推镜像：lan 可达走 registry，否则加密 import 通道
showroom deploy demo v1                                    # 一键部署：push + publish --wait + 大字打印最终 URL（§12.1）
showroom publish demo v1 --wait                            # 仅发布并等待 running，打印完整 URL（项目名可省略=自动随机）
showroom env demo                                          # 查看注入容器的连接信息（DB/Redis）
```

`deploy [项目] <tag> [--mem --cpu --container-port]`：任一步失败即停，报错标明已到第几步
（「deploy: 第 N 步(push|publish) 失败」）；项目名省略时自动生成随机 8 位并醒目打印。

日常命令：`ps`（列表，--json）、`url`、`logs [--tail N] [-f]`、`rollback`、`stop`、`start`、`rm --yes`、`doctor`（体检：密钥/配置/服务端/签名/docker）。
全局 `--server lan|public` 可强制通道；默认自动探测（优先 lan，探不通回落 public）。

说明：

- 所有请求都经 SR-SIG1 签名（SPEC §1），时间偏差需在 ±300s 内。
- import 通道需要管理员提供的平台公钥，放到 `~/.showroom/platform_rsa.pub`（lan 内经 registry 推送则不需要）。

## 环境变量（连接信息）

`publish` 时平台自动向容器注入以下变量，值全部具体化（完整契约见仓库 `SPEC.md` §6）：

```
DB_HOST  DB_PORT  DB_NAME  DB_USER  DB_PASSWORD  DATABASE_URL
REDIS_HOST  REDIS_PORT  REDIS_DB  REDIS_URL
SHOWROOM_PROJECT  SHOWROOM_BASE_PATH
```

随时查看真实值（数据库/Redis 连接地址一目了然）：

```sh
showroom env <project> [slot]
```

应用按 12-factor 读环境变量即可；镜像内配置文件需替换时，推荐入口脚本 `envsubst < config.tpl > config`（模板占位符 `${DB_PASSWORD}` 等在容器启动时被真实值替换）。
