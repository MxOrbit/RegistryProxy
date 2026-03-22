'use strict';

const { resolveUpstream } = require('./config');

/**
 * 处理 Docker Registry 代理请求
 */
async function handleRegistryProxy(req, res, config) {
  // CORS 预检请求
  if (req.method === 'OPTIONS' && req.headers['access-control-request-headers']) {
    res.setHeader('access-control-allow-origin', '*');
    res.setHeader('access-control-allow-methods', 'GET,POST,PUT,PATCH,TRACE,DELETE,HEAD,OPTIONS');
    res.setHeader('access-control-max-age', '1728000');
    return res.status(204).end();
  }

  const fetch = (await import('node-fetch')).default;
  const hubHost = resolveUpstream(req, config);
  const isDockerHub = hubHost === 'registry-1.docker.io';
  const selfUrl = `${req.protocol}://${req.headers.host}`;

  let pathname = req.path;
  let search = buildQueryString(req.query);

  // 处理 %3A 编码的特殊 URL
  const fullUrl = req.originalUrl;
  if (!/%2F/i.test(search) && /%3A/i.test(fullUrl)) {
    const modified = fullUrl.replace(/%3A(?=.*?&)/i, '%3Alibrary%2F');
    const parsed = new URL(modified, selfUrl);
    pathname = parsed.pathname;
    search = parsed.search;
  }

  // Token 请求 -> 转发到对应的 auth 服务
  if (pathname.includes('/token')) {
    const tokenUrl = isDockerHub
      ? `${config.authUrl}${pathname}${search}`
      : `https://${hubHost}${pathname}${search}`;
    const tokenHost = isDockerHub ? 'auth.docker.io' : hubHost;
    const upstream = await fetch(tokenUrl, {
      headers: buildUpstreamHeaders(req, tokenHost),
    });
    const tokenAuthUrl = isDockerHub ? config.authUrl : `https://${hubHost}`;
    return pipeResponse(upstream, res, selfUrl, tokenAuthUrl);
  }

  // 对 Docker Hub 的请求，自动补 library/ 前缀
  if (isDockerHub && /^\/v2\/[^/]+\/[^/]+\/[^/]+$/.test(pathname) && !/^\/v2\/library/.test(pathname)) {
    pathname = '/v2/library/' + pathname.split('/v2/')[1];
  }

  // manifests/blobs/tags 请求：先获取 token 再请求
  if (
    pathname.startsWith('/v2/') &&
    (pathname.includes('/manifests/') || pathname.includes('/blobs/') || pathname.includes('/tags/'))
  ) {
    return handleAuthenticatedRequest(req, res, fetch, hubHost, pathname, search, config, selfUrl, isDockerHub);
  }

  // 普通请求直接转发
  const targetUrl = `https://${hubHost}${pathname}${search}`;
  const headers = buildUpstreamHeaders(req, hubHost);

  if (req.headers['authorization']) {
    headers['Authorization'] = req.headers['authorization'];
  }
  if (req.headers['x-amz-content-sha256']) {
    headers['X-Amz-Content-Sha256'] = req.headers['x-amz-content-sha256'];
  }

  const upstream = await fetch(targetUrl, {
    method: req.method,
    headers,
    redirect: 'manual',
  });

  // 对非 Docker Hub registry，如果返回 401，尝试自动获取 token 重试
  if (!isDockerHub && upstream.status === 401) {
    const wwwAuth = upstream.headers.get('www-authenticate');
    if (wwwAuth) {
      const token = await fetchTokenFromChallenge(fetch, wwwAuth, req);
      if (token) {
        headers['Authorization'] = `Bearer ${token}`;
        const retryRes = await fetch(targetUrl, {
          method: req.method,
          headers,
          redirect: 'manual',
        });
        return pipeResponse(retryRes, res, selfUrl, `https://${hubHost}`, req, hubHost, fetch);
      }
    }
  }

  return pipeResponse(upstream, res, selfUrl, config.authUrl, req, hubHost, fetch);
}

/**
 * 处理需要认证的请求（manifests/blobs/tags）
 */
async function handleAuthenticatedRequest(req, res, fetch, hubHost, pathname, search, config, selfUrl, isDockerHub) {
  // 提取镜像名
  const v2Match = pathname.match(/^\/v2\/(.+?)(?:\/(manifests|blobs|tags)\/)/);
  if (!v2Match) {
    // fallback 到普通请求
    const targetUrl = `https://${hubHost}${pathname}${search}`;
    const upstream = await fetch(targetUrl, {
      method: req.method,
      headers: buildUpstreamHeaders(req, hubHost),
      redirect: 'manual',
    });
    return pipeResponse(upstream, res, selfUrl, config.authUrl, req, hubHost, fetch);
  }

  const repo = v2Match[1];
  let token;

  if (isDockerHub) {
    // Docker Hub: 直接从 auth.docker.io 获取 token
    const tokenUrl = `${config.authUrl}/token?service=registry.docker.io&scope=repository:${repo}:pull`;
    const tokenRes = await fetch(tokenUrl, {
      headers: buildUpstreamHeaders(req, 'auth.docker.io'),
    });
    const tokenData = await tokenRes.json();
    token = tokenData.token;
  } else {
    // 非 Docker Hub registry: 先请求获取 WWW-Authenticate challenge，再获取 token
    const probeUrl = `https://${hubHost}/v2/`;
    const probeRes = await fetch(probeUrl, {
      method: 'GET',
      headers: buildUpstreamHeaders(req, hubHost),
      redirect: 'manual',
    });
    const wwwAuth = probeRes.headers.get('www-authenticate');
    if (wwwAuth) {
      token = await fetchTokenFromChallenge(fetch, wwwAuth, req, repo);
    }
  }

  const targetUrl = `https://${hubHost}${pathname}${search}`;
  const headers = buildUpstreamHeaders(req, hubHost);
  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }

  if (req.headers['x-amz-content-sha256']) {
    headers['X-Amz-Content-Sha256'] = req.headers['x-amz-content-sha256'];
  }

  const upstream = await fetch(targetUrl, {
    method: req.method,
    headers,
    redirect: 'manual',
  });

  return pipeResponse(upstream, res, selfUrl, config.authUrl, req, hubHost, fetch);
}

/**
 * 解析 WWW-Authenticate challenge 并获取 Bearer token
 * 支持格式: Bearer realm="https://...",service="...",scope="..."
 */
async function fetchTokenFromChallenge(fetch, wwwAuth, req, repoOverride) {
  const bearerMatch = wwwAuth.match(/Bearer\s+(.*)/i);
  if (!bearerMatch) return null;

  const params = {};
  const paramRegex = /(\w+)="([^"]+)"/g;
  let m;
  while ((m = paramRegex.exec(bearerMatch[1])) !== null) {
    params[m[1]] = m[2];
  }

  if (!params.realm) return null;

  const tokenUrl = new URL(params.realm);
  if (params.service) tokenUrl.searchParams.set('service', params.service);
  // 如果有实际的 repo 名，始终用它构造 scope（challenge 里的 scope 可能是占位符）
  if (repoOverride) {
    tokenUrl.searchParams.set('scope', `repository:${repoOverride}:pull`);
  } else if (params.scope) {
    tokenUrl.searchParams.set('scope', params.scope);
  }

  try {
    const tokenRes = await fetch(tokenUrl.toString(), {
      headers: buildUpstreamHeaders(req, tokenUrl.hostname),
    });
    if (!tokenRes.ok) return null;
    const data = await tokenRes.json();
    return data.token || data.access_token || null;
  } catch {
    return null;
  }
}

/**
 * 将上游响应 pipe 回客户端，处理 Www-Authenticate 和 Location 重写
 */
async function pipeResponse(upstream, res, selfUrl, authUrl, req, hubHost, fetch) {
  const status = upstream.status;

  // 复制响应头
  const skipHeaders = new Set(['content-encoding', 'transfer-encoding']);
  const removeHeaders = new Set(['content-security-policy', 'content-security-policy-report-only', 'clear-site-data']);
  upstream.headers.forEach((value, key) => {
    const lk = key.toLowerCase();
    if (skipHeaders.has(lk)) return;
    if (removeHeaders.has(lk)) return;

    // 重写 Www-Authenticate
    if (key.toLowerCase() === 'www-authenticate') {
      value = value.replace(new RegExp(authUrl.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'g'), selfUrl);
    }
    res.setHeader(key, value);
  });

  res.setHeader('access-control-allow-origin', '*');
  res.setHeader('access-control-expose-headers', '*');

  // 处理重定向
  const location = upstream.headers.get('location');
  if (location && (status >= 300 && status < 400 || location.startsWith('http'))) {
    if (req && hubHost && fetch) {
      // 跟随重定向并代理（删除 Authorization 修复 S3 错误）
      const redirectHeaders = { ...buildUpstreamHeaders(req, new URL(location).hostname || hubHost) };
      delete redirectHeaders['Authorization'];
      const redirectRes = await fetch(location, {
        method: req.method,
        headers: redirectHeaders,
        redirect: 'follow',
      });
      res.status(redirectRes.status);
      redirectRes.headers.forEach((v, k) => {
        const lk = k.toLowerCase();
        if (skipHeaders.has(lk) || removeHeaders.has(lk)) return;
        res.setHeader(k, v);
      });
      res.setHeader('access-control-allow-origin', '*');
      res.setHeader('access-control-expose-headers', '*');
      res.setHeader('Cache-Control', 'max-age=1500');
      redirectRes.body.pipe(res);
      return;
    }
  }

  res.status(status);
  if (upstream.body) {
    upstream.body.pipe(res);
  } else {
    res.end();
  }
}

/**
 * 构建上游请求头
 */
function buildUpstreamHeaders(req, host) {
  return {
    'Host': host,
    'User-Agent': req.headers['user-agent'] || 'Docker-Client',
    'Accept': req.headers['accept'] || '*/*',
    'Accept-Language': req.headers['accept-language'] || '',
    'Accept-Encoding': req.headers['accept-encoding'] || '',
    'Connection': 'keep-alive',
    'Cache-Control': 'max-age=0',
  };
}

/**
 * 从 req.query 构建 query string
 */
function buildQueryString(query) {
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query || {})) {
    params.set(k, v);
  }
  const str = params.toString();
  return str ? `?${str}` : '';
}

module.exports = { handleRegistryProxy };
