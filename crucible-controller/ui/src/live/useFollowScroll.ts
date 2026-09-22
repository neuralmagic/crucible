import { useLayoutEffect, useRef, useState } from 'react';
import type { RefObject } from 'react';
import { atBottom } from './feed';

export interface FollowScroll {
  scrollRef: RefObject<HTMLDivElement | null>;
  following: boolean;
  onScroll: () => void;
  resume: () => void;
}

/// Auto-follow scrollback over `watch`, the row array the pane renders.
export function useFollowScroll<T>(watch: T, initial = true): FollowScroll {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [following, setFollowing] = useState(initial);

  // Stick to the bottom while following. useLayoutEffect so the jump happens before paint (no flash).
  useLayoutEffect(() => {
    if (!following) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [watch, following]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    setFollowing(atBottom(el.scrollTop, el.scrollHeight, el.clientHeight));
  };

  const resume = () => {
    setFollowing(true);
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  };

  return { scrollRef, following, onScroll, resume };
}
