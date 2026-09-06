import React, { useState } from 'react';
import { Terminal as TerminalIcon, Copy, Check } from 'lucide-react';
import { ExecOutputLine } from '../types';

interface ExecTerminalProps {
  command: string;
  cwd?: string;
  output?: ExecOutputLine[];
  exitCode?: number;
  isRunning?: boolean;
}

export const ExecTerminal: React.FC<ExecTerminalProps> = ({
  command,
  cwd = '~/dev/grodex',
  output = [],
  exitCode,
  isRunning = false,
}) => {
  const [copied, setCopied] = useState(false);

  const handleCopy = () => {
    const text = [
      `$ ${command}`,
      ...output.map((line) => line.text),
      exitCode !== undefined ? `[进程已退出，返回码 ${exitCode}]` : '',
    ].join('\n');
    navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const formatLine = (text: string) => {
    // Gentle muted syntax highlighting for execution output
    if (text.includes('... ok') || text.includes('test result: ok') || text.includes('Finished')) {
      return <span className="text-[#256e2c] font-medium">{text}</span>;
    }
    if (text.includes('FAILED') || text.includes('error:') || text.includes('panicked')) {
      return <span className="text-[#b83838] font-medium">{text}</span>;
    }
    if (text.includes('warning:')) {
      return <span className="text-[#935f12]">{text}</span>;
    }
    if (text.startsWith('$')) {
      return <span className="text-[#2e4d75] font-semibold">{text}</span>;
    }
    if (text.includes('Compiling') || text.includes('Running')) {
      return <span className="text-[#415a77]">{text}</span>;
    }
    return <span className="text-[#3a3f45]">{text}</span>;
  };

  return (
    <div id={`exec-terminal-${command.replace(/\s+/g, '-').slice(0, 20)}`} className="mt-2 rounded-2xl border border-[#e4dfd4] bg-[#fbf9f5] overflow-hidden text-xs font-mono shadow-xs">
      {/* Title Bar */}
      <div className="flex items-center justify-between px-3.5 py-2 bg-[#f4f0e8] border-b border-[#e8e2d7] text-[#555a62]">
        <div className="flex items-center gap-2">
          <TerminalIcon className="w-3.5 h-3.5 text-[#4a5f82]" />
          <span className="text-[#2f353c] font-semibold">{command}</span>
          <span className="text-[#888e98] text-[11px] hidden sm:inline">目录: {cwd}</span>
        </div>
        <div className="flex items-center gap-2">
          {exitCode !== undefined && (
            <span
              className={`px-2 py-0.5 rounded-full text-[10px] font-medium ${
                exitCode === 0
                  ? 'bg-[#eaf5eb] text-[#256e2c] border border-[#d0e9d4]'
                  : 'bg-[#fdf1f1] text-[#b83838] border border-[#f8d4d4]'
              }`}
            >
              退出码 {exitCode}
            </span>
          )}
          {isRunning && (
            <span className="flex items-center gap-1.5 px-2 py-0.5 rounded-full text-[10px] bg-[#fef8ea] text-[#935f12] border border-[#f5dfb4] font-mono">
              <span className="w-1.5 h-1.5 rounded-full bg-[#c9831a] animate-pulse" />
              执行中...
            </span>
          )}
          <button
            id="copy-terminal-output-btn"
            onClick={handleCopy}
            className="p-1 hover:bg-[#eae4d9] rounded-full text-[#6b727c] hover:text-[#2f353c] transition-colors"
            title="复制终端输出"
          >
            {copied ? <Check className="w-3 h-3 text-[#256e2c]" /> : <Copy className="w-3 h-3" />}
          </button>
        </div>
      </div>

      {/* Output Body */}
      <div className="p-3.5 max-h-64 overflow-y-auto space-y-0.5 leading-relaxed bg-[#fbf9f5]">
        {output.length === 0 && isRunning && (
          <div className="text-[#888e98] italic flex items-center gap-2">
            <span className="inline-block w-2 h-3.5 bg-[#4a5f82] animate-cursor-blink rounded-xs" />
            <span>正在沙盒容器中启动进程...</span>
          </div>
        )}

        {output.map((line, idx) => (
          <div key={idx} className="flex items-start gap-2 whitespace-pre-wrap break-all">
            <span className="text-[#a49e92] select-none text-[10px] w-5 text-right shrink-0 pt-0.5">
              {idx + 1}
            </span>
            <div className="flex-1">{formatLine(line.text)}</div>
          </div>
        ))}

        {isRunning && (
          <div className="flex items-center gap-2 pt-1 text-[#4a5f82]">
            <span className="text-[#a49e92] select-none text-[10px] w-5 text-right shrink-0">
              {output.length + 1}
            </span>
            <span className="inline-block w-2 h-3.5 bg-[#4a5f82] animate-cursor-blink rounded-xs" />
          </div>
        )}
      </div>
    </div>
  );
};

