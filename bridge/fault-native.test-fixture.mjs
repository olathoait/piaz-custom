// Test-only: inject one structured quota failure AFTER a real completed native turn.
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { writeFileSync } from 'node:fs';
const args = process.argv.slice(2);
const proxied = args.includes('app-server') && !args.includes('--help');
const child = spawn(process.env.PIAZ_REAL_CODEX, args, { stdio: proxied ? ['pipe','pipe','inherit'] : 'inherit' });
if (proxied) {
  process.stdin.pipe(child.stdin);
  createInterface({ input: child.stdout }).on('line', line => {
    let value; try { value = JSON.parse(line); } catch { process.stdout.write(`${line}\n`); return; }
    if (value.method === 'turn/completed' && value.params.turn.status === 'completed') {
      let inject = false;
      try { writeFileSync(process.env.PIAZ_FAULT_MARKER, JSON.stringify({ threadId:value.params.threadId, turnId:value.params.turn.id }), { flag:'wx', mode:0o600 }); inject = true; } catch {}
      if (inject) {
        const error = { message:'Test-only injected usage limit after completed tool work', codexErrorInfo:'usageLimitExceeded', additionalDetails:null };
        process.stdout.write(`${JSON.stringify({method:'error',params:{threadId:value.params.threadId,turnId:value.params.turn.id,error,willRetry:false}})}\n`);
        value.params.turn = { ...value.params.turn, status:'failed', error };
      }
    }
    process.stdout.write(`${JSON.stringify(value)}\n`);
  });
  process.stdin.on('end',()=>child.kill());
}
child.on('error',()=>process.exit(127)); child.on('exit',code=>process.exit(code??1));
for (const signal of ['SIGTERM','SIGINT']) process.on(signal,()=>child.kill(signal));
