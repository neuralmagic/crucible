import { ansiClasses, ansiParts } from './ansi';

/// One line of program output with its ANSI colours rendered as theme colours and its escapes gone.
export function AnsiText({ text }: { text: string }) {
  return (
    <>
      {ansiParts(text).map((part, i) => {
        const classes = ansiClasses(part);
        return classes === '' ? (
          <span key={i}>{part.text}</span>
        ) : (
          <span key={i} className={classes}>
            {part.text}
          </span>
        );
      })}
    </>
  );
}
