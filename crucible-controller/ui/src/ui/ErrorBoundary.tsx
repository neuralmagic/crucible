import { Component, type ErrorInfo, type ReactNode } from 'react';
import { Button } from './Button';
import { Empty } from './Empty';
import { Mono } from './Mono';

export interface ErrorBoundaryProps {
  children: ReactNode;
}

interface ErrorBoundaryState {
  error: Error | null;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: unknown): ErrorBoundaryState {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error(error, info.componentStack);
  }

  render(): ReactNode {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div role="alert" data-testid="error-boundary">
        <Empty
          title="Page failed"
          description={
            <Mono size="data" tone="ink-3">
              {error.message}
            </Mono>
          }
          action={
            <Button
              variant="filled"
              onClick={() => {
                window.location.reload();
              }}
            >
              Reload
            </Button>
          }
        />
      </div>
    );
  }
}
