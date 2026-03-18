'use strict';

/**
 * Basic Auth 中间件
 * 终止模式：校验通过后清掉 Authorization，不传给后端代理逻辑
 *
 * Docker login 流程：
 *   1. CLI 请求 GET /v2/ → 我们返回 401 + WWW-Authenticate: Basic
 *   2. CLI 带 Authorization: Basic xxx 重试 GET /v2/
 *   3. 我们校验通过 → 直接返回 200 {} （不转发到上游，否则上游会返回 401 Bearer challenge）
 *   4. CLI 认为 login 成功，后续 pull 请求都会带上这个 Basic auth
 */
function basicAuth(config) {
  if (config.authMode !== 'basic') {
    return (_req, _res, next) => next();
  }

  if (!config.authUser || !config.authPass) {
    console.error('AUTH_MODE=basic but AUTH_USER or AUTH_PASS is not set, exiting.');
    process.exit(1);
  }

  const expected = 'Basic ' + Buffer.from(`${config.authUser}:${config.authPass}`).toString('base64');

  return (req, res, next) => {
    // 健康检查跳过认证
    if (req.path === '/healthz') return next();

    const auth = req.headers['authorization'];
    if (!auth || auth !== expected) {
      res.setHeader('WWW-Authenticate', 'Basic realm="Registry Proxy"');
      return res.status(401).json({ errors: [{ code: 'UNAUTHORIZED', message: 'authentication required' }] });
    }

    // 认证通过。对 /v2/ 精确路径直接返回 200，
    // 不转发到上游（上游会返回 401 Bearer challenge 导致 docker login 失败）
    if (req.path === '/v2/' || req.path === '/v2') {
      return res.status(200).json({});
    }

    // 其他路径：清掉 Authorization，继续走代理
    delete req.headers['authorization'];
    next();
  };
}

module.exports = { basicAuth };
