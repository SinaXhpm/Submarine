import { useRef } from "react";

// A dialog's backdrop that closes it on a click beside it, but only when the
// press began there too. A text selection dragged out of a field and let go
// beside the dialog ends in a click on the backdrop (the press and the release
// share no closer parent), and that must not close the dialog and lose what
// was typed. A click made by a script (the lab tests close dialogs that way)
// has no press, and counts.
export function useBackdropClose(close: () => void) {
  const pressedBesideRef = useRef(false);
  return {
    onPointerDown: (e: React.PointerEvent<HTMLElement>) => {
      pressedBesideRef.current = e.target === e.currentTarget;
    },
    onClick: (e: React.MouseEvent<HTMLElement>) => {
      const beside = pressedBesideRef.current || !e.nativeEvent.isTrusted;
      pressedBesideRef.current = false;
      if (e.target === e.currentTarget && beside) close();
    },
  };
}
