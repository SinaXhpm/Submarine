import DesktopApp from "./DesktopApp";
import { ConfirmProvider } from "./ui/confirm";
import { BroadcastProvider } from "./ui/broadcast";
import { useT } from "./i18n";

function App() {
  // A language change renders the whole app again (src/i18n).
  useT();
  return (
    <ConfirmProvider>
      <BroadcastProvider>
        <DesktopApp />
      </BroadcastProvider>
    </ConfirmProvider>
  );
}

export default App;
