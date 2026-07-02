import DesktopApp from "./DesktopApp";
import { ConfirmProvider } from "./ui/confirm";
import { BroadcastProvider } from "./ui/broadcast";

function App() {
  return (
    <ConfirmProvider>
      <BroadcastProvider>
        <DesktopApp />
      </BroadcastProvider>
    </ConfirmProvider>
  );
}

export default App;
