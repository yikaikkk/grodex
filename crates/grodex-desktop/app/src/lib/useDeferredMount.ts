import { useEffect, useState } from 'react';

/**
 * Two-phase modal mount. `useEffect` runs after the browser has painted, so
 * returning true one task later means the (cheap) overlay shell is already
 * on screen when the (potentially heavy) panel body mounts. Click-to-visible
 * latency becomes one frame regardless of how heavy the modal content is.
 */
export function useDeferredMount(isOpen: boolean): boolean {
  const [mounted, setMounted] = useState(false);
  useEffect(() => {
    if (!isOpen) {
      setMounted(false);
      return;
    }
    const t = setTimeout(() => setMounted(true), 0);
    return () => clearTimeout(t);
  }, [isOpen]);
  return mounted;
}
