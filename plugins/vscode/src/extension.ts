import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

import { loadCatalogJson } from "./catalog";
import { BindfettoDecoder } from "./decoder";

let decoder: BindfettoDecoder | undefined;

/** Load (or reuse) the decoder from the configured catalog + bundled wasm. */
async function getDecoder(context: vscode.ExtensionContext): Promise<BindfettoDecoder | undefined> {
  if (decoder) {
    return decoder;
  }
  const catalogPath = vscode.workspace.getConfiguration("bindfetto").get<string>("catalogPath");
  if (!catalogPath) {
    vscode.window.showErrorMessage(
      'bindfetto: set "bindfetto.catalogPath" to your AIDL catalog JSON.'
    );
    return undefined;
  }
  const wasmPath = path.join(context.extensionPath, "media", "bindfetto_decode_wasm.wasm");
  const wasmBytes = fs.readFileSync(wasmPath);
  const catalogJson = loadCatalogJson(catalogPath);
  decoder = await BindfettoDecoder.load(wasmBytes, catalogJson);
  return decoder;
}

export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("bindfetto.decodeActiveEditor", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) {
        vscode.window.showErrorMessage("bindfetto: no active editor.");
        return;
      }
      try {
        const dec = await getDecoder(context);
        if (!dec) {
          return;
        }
        const decoded = editor.document
          .getText()
          .split("\n")
          .map((line) => dec.decodeLine(line))
          .join("\n");
        const doc = await vscode.workspace.openTextDocument({
          content: decoded,
          language: editor.document.languageId,
        });
        await vscode.window.showTextDocument(doc, vscode.ViewColumn.Beside);
      } catch (err) {
        vscode.window.showErrorMessage(`bindfetto: ${err instanceof Error ? err.message : err}`);
      }
    }),

    // Rebuild the decoder if the catalog path changes.
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("bindfetto.catalogPath")) {
        decoder?.dispose();
        decoder = undefined;
      }
    })
  );
}

export function deactivate(): void {
  decoder?.dispose();
  decoder = undefined;
}
