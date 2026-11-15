// Managed preview owns this development process; production hosting is static.
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';
const port = process.env.PORT || '4173';
const executable = [process.env.TRANSCRIPTION_SERVER_EXE, 'TranscriptionServer.exe', 'target/release/TranscriptionServer.exe', 'target/debug/TranscriptionServer.exe'].filter(Boolean).map(p => path.resolve(p)).find(existsSync);
if (!executable) throw new Error('Rust版をcargo buildで構築するか、TRANSCRIPTION_SERVER_EXEを指定してください。');
const child = spawn(executable, ['--local', '--host', '127.0.0.1', '--port', port, '--no-browser', '--no-auto-runtimes', '--threads', '4', '--data-dir', path.resolve('.preview-data')], { stdio: 'inherit' });
child.on('error', error => { console.error(error.message); process.exitCode = 1; });
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
child.on('exit', code => process.exit(code || 0));
