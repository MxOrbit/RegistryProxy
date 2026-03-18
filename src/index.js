'use strict';

const express = require('express');
const { handleRegistryProxy } = require('./proxy');
const { getConfig } = require('./config');
const { basicAuth } = require('./auth');

const app = express();
const config = getConfig();

// 健康检查（在 auth 之前）
app.get('/healthz', (_req, res) => res.send('ok'));

// Basic Auth 中间件
app.use(basicAuth(config));

// 主路由
app.all('*', async (req, res) => {
  try {
    const ua = (req.headers['user-agent'] || '').toLowerCase();

    // 屏蔽爬虫 UA
    if (config.blockedUA.some(blocked => ua.includes(blocked))) {
      return res.status(404).end();
    }

    // 所有请求走 registry 代理
    await handleRegistryProxy(req, res, config);
  } catch (err) {
    console.error('Request error:', err);
    if (!res.headersSent) {
      res.status(502).json({ error: 'Bad Gateway', message: err.message });
    }
  }
});

const PORT = config.port;
app.listen(PORT, '0.0.0.0', () => {
  console.log(`Registry Proxy listening on port ${PORT}`);
  console.log(`Mode: ${config.mode}`);
  console.log(`Auth: ${config.authMode}`);
  if (config.mode === 'single') {
    console.log(`Upstream: ${config.upstream}`);
  } else {
    console.log('Route mode: routing by subdomain prefix');
  }
});
