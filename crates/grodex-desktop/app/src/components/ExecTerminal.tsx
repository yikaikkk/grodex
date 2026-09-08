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
    // Gentle muted syntax highlighting for execution output.
    if (text.includes('... ok') || text.includes('test result: ok') || text.includes('Finished')) {
      return <span className="text-terminal-green font-medium">{text}</span>;
    }
    if (text.includes('FAILED') || text.includes('error:') || text.includes('panicked')) {
      return <span className="text-terminal-red font-medium">{text}</span>;
    }
    if (text.includes('warning:')) {
      return <span className="text-terminal-yellow">{text}</span>;
    }
    if (text.startsWith('$')) {
      return <span className="text-terminal-blue font-semibold">{text}</span>;
    }
    if (text.includes('Compiling') || text.includes('Running')) {
      return <span className="text-terminal-blue">{text}</span>;
    }
    return <span className="text-terminal-line">{text}</span>;
  };

  return (
    <div
      id={`exec-terminal-${command.replace(/\s+/g, '-').slice(0, 20)}`}
      className="mt-2 rounded-xl bg-terminal overflow-hidden text-xs font-mono shadow-sm ring-1 ring-black/5"
    >
      {/* Title Bar */}
      <div className="flex items-center justify-between px-3.5 py-2 bg-black/20 border-b border-white/[0.06]">
        <div className="flex items-center gap-2 min-w-0">
          <TerminalIcon className="w-3.5 h-3.5 text-terminal-muted shrink-0" />
          <span className="text-terminal-line font-medium truncate">{command}</span>
          <span className="text-terminal-muted text-[11px] hidden sm:inline shrink-0">目录: {cwd}</span>
        </div>
        <div className="flex items-center gap-2">
          {exitCode !== undefined && (
            <span
              className={`px-2 py-0.5 rounded-full text-[10px] font-medium ${
                exitCode === 0
                  ? 'bg-terminal-green/15 text-terminal-green'
                  : 'bg-terminal-red/15 text-terminal-red'
              }`}
            >
              退出码 {exitCode}
            </span>
          )}
          {isRunning && (
            <span className="flex items-center gap-1.5 px-2 py-0.5 rounded-full text-[10px] bg-terminal-yellow/15 text-terminal-yellow font-mono">
              <span className="w-1.5 h-1.5 rounded-full bg-terminal-yellow animate-pulse" />
              执行中...
            </span>
          )}
          <button
            id="copy-terminal-output-btn"
            onClick={handleCopy}
            className="p-1 hover:bg-white/[0.08] rounded-md text-terminal-muted hover:text-terminal-line transition-colors"
            title="复制终端输出"
          >
            {copied ? <Check className="w-3 h-3 text-terminal-green" /> : <Copy className="w-3 h-3" />}
          </button>
        </div>
      </div>

      {/* Output Body */}
      <div className="p-3.5 max-h-64 overflow-y-auto space-y-0.5 leading-relaxed">
        {output.length === 0 && isRunning && (
          <div className="text-terminal-muted italic flex items-center gap-2">
            <span className="inline-block w-2 h-3.5 bg-terminal-blue animate-cursor-blink rounded-xs" />
            <span>正在沙盒容器中启动进程...</span>
          </div>
        )}

        {output.map((line, idx) => (
          <div key={idx} className="flex items-start gap-2 whitespace-pre-wrap break-all">
            <span className="text-terminal-muted/60 select-none text-[10px] w-5 text-right shrink-0 pt-0.5">
              {idx + 1}
            </span>
            <div className="flex-1">{formatLine(line.text)}</div>
          </div>
        ))}

        {isRunning && (
          <div className="flex items-center gap-2 pt-1">
            <span className="text-terminal-muted/60 select-none text-[10px] w-5 text-right shrink-0">
              {output.length + 1}
            </span>
            <span className="inline-block w-2 h-3.5 bg-terminal-blue animate-cursor-blink rounded-xs" />
          </div>
        )}
      </div>
    </div>
  );
};
