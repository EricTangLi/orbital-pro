import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import pkg from './package.json'

// T-M10-09：设置页「关于」的版本号原来是 `App.tsx` 里硬编码的 `0.72.0`，
// 打包升到 0.90.x 后界面仍显示旧号（用户实测发现）。
// 改为构建期从 package.json 注入，消灭这个漂移点（tsconfig 已开 resolveJsonModule）。
const APP_VERSION = (pkg as { version: string }).version

// Tauri 相关固定：不要让 Vite 清屏（会盖住 Rust 侧日志），端口固定 5173
export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(APP_VERSION),
  },
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    // ⚠️ 本机实测（2026-11-12）：默认监听 `localhost` 会解析到 `::1`（IPv6 回环），
    // 而本机该地址不可用 -> `EADDRNOTAVAIL: listen ::1:5173`，Vite 起不来。
    // 显式钉到 IPv4 回环。
    host: '127.0.0.1',
    // ⚠️ 2026-11-12 实测：5173 落在 Windows 端口排除范围 5097-5196 内
    // （被 Hyper-V/WSL 保留）-> `EACCES: listen 127.0.0.1:5173`，Vite 起不来。
    // 换到排除范围之外的 14321。
    port: 14321,
    strictPort: true,
  },
  build: {
    target: 'chrome110',
    sourcemap: false,
  },
})
