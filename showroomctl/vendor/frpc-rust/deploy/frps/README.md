# 将 frps 对外控制端口改为 7001

将本目录的 `compose.yaml` 和 `frps.toml` 放到服务端同一个目录。此配置使用 Docker 默认桥接网络，端口关系为：

| 用途 | 宿主机端口 | 容器内端口 |
| --- | --- | --- |
| frpc 控制连接 | 7001/TCP | 7000/TCP |
| 配套 web 代理 | 18080/TCP | 18080/TCP |

`7001:7000/tcp` 左侧是客户端访问的宿主机端口，右侧对应 `frps.toml` 的 `bindPort`。已被其他应用占用的宿主机 7000 不会被此配置绑定。不要在此 Compose 中添加 `network_mode: host`，否则端口发布规则不生效。

在服务端执行：

```sh
export FRP_TOKEN='替换为与客户端一致的Token'
docker compose config -q
docker compose up -d frps
docker compose logs --tail=100 frps
```

Compose 会在缺少 `FRP_TOKEN` 时直接报错。也可以通过服务端部署环境持久化这个变量；不要将实际 Token 提交到仓库。`frps.toml` 使用官方支持的环境变量模板读取容器内的 Token。

客户端把 `serverPort` 改为 **7001**，例如：

```toml
serverAddr = "192.168.5.21"
serverPort = 7001
auth.token = "{{ .Envs.FRP_TOKEN }}"
transport.protocol = "tcp"
transport.tls.enable = true
```

运行客户端前也需要在客户端环境中设置相同的 `FRP_TOKEN`。根目录开发示例原有的代理配置可以保留。

添加 TCP/UDP 代理时，还需要在 Compose 的 `ports` 中发布相应 `remotePort`。HTTP/HTTPS 域名代理则需要另外配置并发布 `vhostHTTPPort`/`vhostHTTPSPort`；这里未启用管理页面或其他入口。

本配置只提供部署文件，没有在当前 Windows 工作区启动 Docker 或修改远端服务器。
