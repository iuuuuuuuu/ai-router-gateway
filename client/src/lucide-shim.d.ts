// @lucide/vue ships types at dist/lucide-vue.d.ts but declares no "types"
// field in package.json, so TS cannot resolve it. Build uses esbuild (no type
// check), this shim just keeps the editor quiet.
declare module "@lucide/vue";
