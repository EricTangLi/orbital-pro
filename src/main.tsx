import React from 'react'
import ReactDOM from 'react-dom/client'
// 字体随包内置（docs/10 §6），不依赖网络字体
import '@fontsource-variable/inter'
import '@fontsource-variable/jetbrains-mono'
import App from './App'
import PanelView from './PanelView'
import './styles.css'

// T-M8-01：`?panel=1` 是托盘右键的自绘面板（ADR-016）。以前这个参数没被消费，
// 右键托盘会把整个应用塞进 320×420 的窗口。
const isPanel = new URLSearchParams(window.location.search).get('panel') === '1'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>{isPanel ? <PanelView /> : <App />}</React.StrictMode>,
)