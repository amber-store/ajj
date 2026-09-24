import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
const root = new URL('./', import.meta.url);
const types = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json' };
const allowed = new Set(['index.html', 'style.css', 'app.js', 'repository.json']);
createServer(async (req, res) => {
  const name = new URL(req.url, 'http://localhost').pathname.slice(1) || 'index.html';
  if (!allowed.has(name)) { res.writeHead(404).end('Not found'); return; }
  try {
    const body = await readFile(new URL(name, root));
    res.writeHead(200, { 'Content-Type': `${types[name.slice(name.lastIndexOf('.'))]}; charset=utf-8`, 'X-Content-Type-Options': 'nosniff' }).end(body);
  } catch { res.writeHead(500).end('Unable to load frontend'); }
}).listen(Number(process.env.PORT || 3000), '127.0.0.1', () => console.log('ajj forge: http://localhost:3000'));
