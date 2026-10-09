// S0b parent-owned loopback HTTP server (auth-proxy stand-in).
// usage: node net-http.js <port>
const http = require('http');
const port = Number(process.argv[2]);
if (!port) { console.error('usage: net-http.js <port>'); process.exit(2); }
const srv = http.createServer((req, res) => {
  res.writeHead(200, { 'content-type': 'text/plain' });
  res.end('AUTH-PROXY-OK\n');
});
srv.listen(port, '127.0.0.1', () => console.log('listening ' + port));
process.on('SIGTERM', () => srv.close(() => process.exit(0)));
