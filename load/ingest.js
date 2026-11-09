// k6 ingest test for the gateway. Signs each body like the gateway expects:
// header "t=<unix seconds>,v1=<hex>", where v1 is HMAC-SHA256 of "<t>.<body>".
// Env: BASE, API_KEY, SECRET, RATE (requests per second), DURATION, PRE_VUS, MAX_VUS.
import http from 'k6/http';
import crypto from 'k6/crypto';
import { check } from 'k6';

const BASE = __ENV.BASE || 'http://127.0.0.1:25680';
const API_KEY = __ENV.API_KEY || 'load-key';
const SECRET = __ENV.SECRET || 'load-secret';
const RATE = parseInt(__ENV.RATE || '500');
const DURATION = __ENV.DURATION || '20s';

export const options = {
  scenarios: {
    ingest: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: parseInt(__ENV.PRE_VUS || '50'),
      maxVUs: parseInt(__ENV.MAX_VUS || '200'),
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.01'],
  },
};

export default function () {
  const body = JSON.stringify({
    type: 'order.created',
    order_id: `${__VU}-${__ITER}-${Date.now()}`,
    amount: 12.5,
    customer: { email: 'load@example.com' },
  });
  const t = Math.floor(Date.now() / 1000);
  const sig = crypto.hmac('sha256', SECRET, `${t}.${body}`, 'hex');
  const res = http.post(`${BASE}/v1/webhooks`, body, {
    headers: {
      'Content-Type': 'application/json',
      'X-Api-Key': API_KEY,
      'X-Signature': `t=${t},v1=${sig}`,
      'Idempotency-Key': `k6-${__VU}-${__ITER}-${Date.now()}`,
    },
  });
  check(res, { 'accepted 202': (r) => r.status === 202 });
}
