// Production: a static export (`out/`) that the rustkubeadmin binary embeds and serves itself — no Node.js at run time.
// Development: `npm run dev` serves the pages and forwards /api to a running rustkubeadmin (RUSTKUBEADMIN_API, default
// http://127.0.0.1:8090; start that with RUSTKUBEADMIN_DEV_ORIGIN=http://localhost:3000 so it accepts the dev origin).
const dev = process.env.NODE_ENV === "development";
const api = process.env.RUSTKUBEADMIN_API || "http://127.0.0.1:8090";

/** @type {import('next').NextConfig} */
const config = dev
  ? { async rewrites() { return [{ source: "/api/:path*", destination: `${api}/api/:path*` }]; } }
  : { output: "export", images: { unoptimized: true } };

export default config;
