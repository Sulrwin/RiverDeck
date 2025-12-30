/// <reference lib="deno.ns" />
// deno-lint-ignore-file no-import-prefix

import { copy } from "jsr:@std/fs@^1.0";
import { dirname, fromFileUrl, join } from "jsr:@std/path@^1.0";

if (Deno.args.length < 2) Deno.exit(1);
const outDir = Deno.args[0];
const target = Deno.args[1];
const scriptDir = dirname(fromFileUrl(import.meta.url));

try {
	await Deno.remove(outDir, { recursive: true });
} catch (error: any) {
	if (!(error instanceof Deno.errors.NotFound)) {
		throw error;
	}
}

await copy(join(scriptDir, "assets"), outDir);
if (
	!(
		await new Deno.Command("cargo", {
			cwd: scriptDir,
			args: ["install", "--path", scriptDir, "--target", target, "--root", join(outDir, Deno.build.os)],
		}).spawn().status
	).success
) Deno.exit(1);
