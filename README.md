# RegistryProxy

[![Dockerhub](https://img.shields.io/docker/pulls/mmx233/registry-proxy)](https://hub.docker.com/repository/docker/mmx233/registry-proxy)

自托管的轻量 Docker Registry 镜像代理服务。

## 快速开始

### Docker Run

```bash
docker run -d \
  --name registry-proxy \
  --restart unless-stopped \
  -p 3000:3000 \
  -e MODE=single \
  -e UPSTREAM=registry-1.docker.io \
  mmx233/registry-proxy:latest
```

### 自行构建

```bash
git clone https://github.com/MxOrbit/RegistryProxy.git
cd RegistryProxy
docker build -t registry-proxy .
docker run -d -p 3000:3000 registry-proxy
```

## 环境变量

| 变量           | 默认值                      | 说明                                    |
|--------------|--------------------------|---------------------------------------|
| `PORT`       | `3000`                   | 监听端口                                  |
| `MODE`       | `single`                 | `single` 单一上游 / `route` 根据子域名路由       |
| `UPSTREAM`   | `registry-1.docker.io`   | 单一模式下的上游地址                            |
| `AUTH_URL`   | `https://auth.docker.io` | Docker 认证服务器                          |
| `BLOCKED_UA` | `netcraft`               | 屏蔽的 User-Agent（逗号/空格分隔）               |
| `AUTH_MODE`  | `passthrough`            | `passthrough` 不做认证 / `basic` 终止认证     |
| `AUTH_USER`  | -                        | Basic Auth 用户名（`AUTH_MODE=basic` 时必填） |
| `AUTH_PASS`  | -                        | Basic Auth 密码（`AUTH_MODE=basic` 时必填）  |
| `ROUTES`     | -                        | 自定义路由表 JSON，`MODE=route` 时生效          |

## 使用方式

假设你的服务部署在 `https://docker.example.com`：

### 直接拉取

```bash
docker pull docker.example.com/library/nginx:latest
docker pull docker.example.com/stilleshan/frpc:latest
```

### 配置为镜像加速

```bash
sudo tee /etc/docker/daemon.json <<-'EOF'
{
  "registry-mirrors": ["https://docker.example.com"]
}
EOF
sudo systemctl daemon-reload
sudo systemctl restart docker
```

### 路由模式

设置 `MODE=route` 后，可通过子域名前缀路由到不同 registry：

| 子域名前缀        | 上游 Registry          |
|--------------|----------------------|
| `quay`       | quay.io              |
| `gcr`        | gcr.io               |
| `k8s-gcr`    | k8s.gcr.io           |
| `k8s`        | registry.k8s.io      |
| `ghcr`       | ghcr.io              |
| `cloudsmith` | docker.cloudsmith.io |
| `nvcr`       | nvcr.io              |

可通过 `ROUTES` 环境变量扩展路由表。

## 反向代理

Nginx

```nginx
server {
    listen 443 ssl;
    server_name docker.example.com;

    ssl_certificate     /path/to/cert.pem;
    ssl_certificate_key /path/to/key.pem;

    client_max_body_size 0;
    chunked_transfer_encoding on;

    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
        proxy_request_buffering off;
    }
}
```
