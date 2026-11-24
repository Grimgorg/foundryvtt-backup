// Renders app-icon.svg to a 1024×1024 app-icon.png (input for `tauri icon`)
// and a small public/logo.png used in the app header.
import { Resvg } from "@resvg/resvg-js";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";

const svg = readFileSync(new URL("../app-icon.svg", import.meta.url));
const render = (width) => new Resvg(svg, { fitTo: { mode: "width", value: width } }).render().asPng();

writeFileSync(new URL("../app-icon.png", import.meta.url), render(1024));
mkdirSync(new URL("../public/", import.meta.url), { recursive: true });
writeFileSync(new URL("../public/logo.png", import.meta.url), render(160));
console.log("Wrote app-icon.png and public/logo.png");
