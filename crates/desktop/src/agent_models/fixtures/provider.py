# Synthetic loopback HTTP provider. No external network or live credentials.
import http.server, json, sys, threading, urllib.parse
from pathlib import Path
log = Path(sys.argv[1])
state = {"grants": 0, "requests": [], "inferences": 0}
lock = threading.Lock()
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def send(self, status, body):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)
    def do_GET(self):
        base = 'http://127.0.0.1:' + str(self.server.server_port)
        if self.path.startswith('/oidc/'):
            return self.send(200, {"authorization_endpoint":base+'/authorize', "token_endpoint":base+'/token'})
        with lock:
            state['requests'].append([self.path, self.headers.get('Authorization')])
            log.write_text(json.dumps(state))
        rejected = len(sys.argv) > 2 and sys.argv[2] == 'catalog-rejection' and self.headers.get('Authorization') == 'Bearer synthetic-1'
        if rejected or self.headers.get('Authorization') != 'Bearer synthetic-'+str(state['grants']):
            return self.send(401, {'error':'synthetic rejection'})
        if self.path.startswith('/api/ai-gateway/v2/endpoints'):
            return self.send(200, {'endpoints':[{'name':'synthetic-model'}]})
        return self.send(200, {'model_services':[]})
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length','0')))
        if self.path == '/token':
            form = urllib.parse.parse_qs(raw.decode())
            assert form['client_id'] == ['databricks-cli']
            assert form['grant_type'][0] in ['authorization_code', 'refresh_token']
            if len(sys.argv) > 2 and sys.argv[2] == 'catalog-rejection' and form['grant_type'] == ['refresh_token']:
                return self.send(400, {'error': 'invalid_grant'})
            with lock:
                state['grants'] += 1
                state['requests'].append(['grant', form['grant_type'][0]])
                log.write_text(json.dumps(state))
            return self.send(200, {'access_token':'synthetic-'+str(state['grants']), 'refresh_token':'synthetic-refresh', 'expires_in':3600})
        with lock:
            state['inferences'] += 1
            state['requests'].append([self.path,self.headers.get('Authorization')])
            body = json.loads(raw)
            state['model'] = body.get('model')
            state['tools'] = [t.get('function', {}).get('name') for t in body.get('tools', [])]
            log.write_text(json.dumps(state))
        # Force a server rejection on the first inference despite unexpired cache.
        if state['inferences'] == 1:
            return self.send(401, {'error': {'message':'synthetic expired token'}})
        assert self.headers.get('Authorization') == 'Bearer synthetic-'+str(state['grants'])
        return self.send(200, {'id':'synthetic', 'object':'chat.completion', 'choices':[{'index':0,'finish_reason':'stop','message':{'role':'assistant','content':'SYNTHETIC_INFERENCE_OK'}}], 'usage':{'prompt_tokens':8,'completion_tokens':4,'total_tokens':12}})
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print('http://127.0.0.1:' + str(server.server_port), flush=True)
server.serve_forever()
