import { ExtensionContext, commands, window, workspace } from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions } from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

function serverPath(): string {
  const configured = workspace.getConfiguration('crucible').get<string>('server.path', '');
  return configured === '' ? 'crucible-lsp' : configured;
}

async function start(): Promise<void> {
  const command = serverPath();
  const serverOptions: ServerOptions = { run: { command }, debug: { command } };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'crucible-starlark' },
      { scheme: 'untitled', language: 'crucible-starlark' },
    ],
  };
  client = new LanguageClient('crucible-lsp', 'Crucible', serverOptions, clientOptions);
  try {
    await client.start();
  } catch (error) {
    client = undefined;
    void window.showErrorMessage(`crucible-lsp (${command}) failed to start: ${String(error)}`);
  }
}

async function restart(): Promise<void> {
  await client?.stop();
  await start();
}

export async function activate(context: ExtensionContext): Promise<void> {
  context.subscriptions.push(
    commands.registerCommand('crucible.restartServer', restart),
    workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('crucible.server.path')) {
        void restart();
      }
    }),
  );
  await start();
}

export async function deactivate(): Promise<void> {
  await client?.stop();
}
