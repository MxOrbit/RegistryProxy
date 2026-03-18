'use strict';

/**
 * Basic Auth 中间件
 * 终止模式：校验通过后清掉 Authorization，不传给后端代理逻辑
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

    // 认证通过，清掉 Authorization，不传给上游代理逻辑
    delete req.headers['authorization'];
    next();
  };
}

module.exports = { basicAuth };
