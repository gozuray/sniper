/**
 * PM2 ecosystem config for the sniper Rust binary.
 *
 * On the server (e.g. /root/sniper):
 *   1. Build first:  cargo build --release
 *   2. Start PM2:    pm2 start ecosystem.config.cjs
 *
 * Script must point to the compiled binary, not the project directory.
 */
module.exports = {
  apps: [
    {
      name: "sniper",
      script: "./target/release/sniper",
      interpreter: "none",
      cwd: "/root/sniper",
      exec_mode: "fork",
      env: {},
    },
  ],
};
