# Local Archive Lab

Vue 3 + Rust 的本地 ZipCrypto 恢复工具。它使用 `bkcrack` 的已知明文攻击恢复内部密钥，然后并发解密外层归档中的 A-F 条目；F 题的裸 Deflate 数据由后续工具还原为 ZIP。

## 启动

1. `bkcrack.exe` 已随仓库放在 `tools\bkcrack.exe`；也可以通过 `BKCRACK_PATH` 指向其他版本。
2. 启动后端：`cd backend; cargo run`
3. 启动前端：`cd frontend; npm install; npm run dev`
4. 浏览器打开 `http://localhost:5173`。

输入文件必须是你有权访问的 ZipCrypto ZIP。应用不会上传归档；后端只在本机读取路径和写出结果。

浏览器安全模型不会把文件选择器的 Windows 绝对路径交给网页，因此请把完整路径粘贴到“原始 ZIP 路径”输入框；选择文件按钮只能帮助确认文件名。若需要真正的系统文件选择器，可把此 Vue 前端封装到 Tauri。

程序恢复的是 ZipCrypto 内部密钥（三组 32 位值），这足以解密归档，但不保证能唯一反推出当初设置的字符串密码。
