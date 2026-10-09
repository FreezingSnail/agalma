// S0b parent-owned Unix domain socket server (nerve-conductor bridge stand-in).
// usage: node net-uds.js <socket-path>
const net = require('net');
const fs = require('fs');
const p = process.argv[2];
if (!p) { console.error('usage: net-uds.js <socket-path>'); process.exit(2); }
try { fs.unlinkSync(p); } catch (e) {}
const srv = net.createServer((c) => {
  c.setEncoding('utf8');
  c.on('data', (d) => c.write('NERVE-OK:' + d.trim() + '\n'));
});
srv.listen(p, () => console.log('listening ' + p));
process.on('SIGTERM', () => srv.close(() => process.exit(0)));
