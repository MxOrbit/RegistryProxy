'use strict';

// 路由表：子域名前缀 -> 上游 registry
const ROUTE_TABLE = {
  'quay':       'quay.io',
  'gcr':        'gcr.io',
  'k8s-gcr':    'k8s.gcr.io',
  'k8s':        'registry.k8s.io',
  'ghcr':       'ghcr.io',
  'cloudsmith': 'docker.cloudsmith.io',
  'nvcr':       'nvcr.io',
};

function getConfig() {
  const blockedUAStr = process.env.BLOCKED_UA || 'netcraft';
  const blockedUA = blockedUAStr.split(/[\s,|]+/).filter(Boolean).map(s => s.toLowerCase());

  // MODE: 'single' (单一上游) 或 'route' (根据子域名路由)
  const mode = process.env.MODE || 'single';

  // 单一模式下的上游地址，默认 Docker Hub
  const upstream = process.env.UPSTREAM || 'registry-1.docker.io';

  // 自定义路由表（JSON 格式），会合并到默认路由表
  let customRoutes = {};
  if (process.env.ROUTES) {
    try {
      customRoutes = JSON.parse(process.env.ROUTES);
    } catch (e) {
      console.error('Failed to parse ROUTES env:', e.message);
    }
  }

  return {
    port: parseInt(process.env.PORT || '3000', 10),
    mode,
    upstream,
    routes: { ...ROUTE_TABLE, ...customRoutes },
    blockedUA,
    authUrl: process.env.AUTH_URL || 'https://auth.docker.io',
  };
}

/**
 * 根据请求确定上游 registry 地址
 */
function resolveUpstream(req, config) {
  // 优先使用 ns query 参数
  const ns = req.query.ns;
  if (ns) {
    return ns === 'docker.io' ? 'registry-1.docker.io' : ns;
  }

  // 支持 hubhost query 参数覆盖 hostname（与原版兼容）
  const hostname = req.query.hubhost || req.hostname || '';
  const prefix = hostname.split('.')[0];

  if (config.mode === 'route') {
    if (prefix in config.routes) {
      return config.routes[prefix];
    }
  }

  return config.upstream;
}

module.exports = { getConfig, resolveUpstream };
