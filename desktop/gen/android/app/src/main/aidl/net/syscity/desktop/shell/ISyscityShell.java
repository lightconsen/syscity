/*
 * This file is auto-generated.  DO NOT MODIFY.
 * Using: /opt/homebrew/share/android-commandlinetools/build-tools/35.0.0/aidl -p /opt/homebrew/share/android-commandlinetools/platforms/android-36/framework.aidl -I desktop/gen/android/app/src/main/aidl desktop/gen/android/app/src/main/aidl/net/syscity/desktop/shell/ISyscityShell.aidl
 */
package net.syscity.desktop.shell;
/**
 * The display helper's Binder interface, as seen from the app.
 * 
 * The helper runs in the `shell` SELinux domain (it is started through
 * `app_process` from `adb shell`, which is the only way to reach
 * `CAPTURE_VIDEO_OUTPUT`). An untagged app cannot open a unix socket there —
 * `avc: denied { connectto } untrusted_app → shell` — but it *can* receive a
 * Binder from it through a ContentProvider, which is a direction Android's
 * policy allows. So the helper hands this interface over once at start-up and
 * everything afterwards is ordinary Binder calls.
 * 
 * Frames cross as a `ParcelFileDescriptor`-backed temp file rather than a byte
 * array: a Binder transaction is capped at 1 MB and a 1080x2400 PNG is not.
 */
public interface ISyscityShell extends android.os.IInterface
{
  /** Default implementation for ISyscityShell. */
  public static class Default implements net.syscity.desktop.shell.ISyscityShell
  {
    /** `[uid, pid]` of the process this Binder lives in; checked before it is accepted. */
    @Override public int[] identity() throws android.os.RemoteException
    {
      return null;
    }
    /** Create a display and return its id, or -1. */
    @Override public int createDisplay(int width, int height, int dpi) throws android.os.RemoteException
    {
      return 0;
    }
    /** Put `package` on the display. Returns 0 on success. */
    @Override public int startApp(int displayId, java.lang.String packageName) throws android.os.RemoteException
    {
      return 0;
    }
    /** Destroy a display and everything on it. */
    @Override public void releaseDisplay(int displayId) throws android.os.RemoteException
    {
    }
    /** Tap at a point on the display. */
    @Override public boolean injectTap(int displayId, int x, int y) throws android.os.RemoteException
    {
      return false;
    }
    /** Swipe on the display. */
    @Override public boolean injectSwipe(int displayId, int x1, int y1, int x2, int y2, int durationMs) throws android.os.RemoteException
    {
      return false;
    }
    /** Press a key on the display. */
    @Override public boolean injectKey(int displayId, int keyCode) throws android.os.RemoteException
    {
      return false;
    }
    /** Type text into whatever has focus on the display. */
    @Override public boolean injectText(int displayId, java.lang.String text) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * The display's current frame, written to a PNG file.
     * 
     * Returns the file's path — the reader is the app's own process and opens it
     * directly — or null when the display produced nothing.
     */
    @Override public java.lang.String frame(int displayId) throws android.os.RemoteException
    {
      return null;
    }
    /** The app's own debug surface: which overload created this display, for logs. */
    @Override public java.lang.String describe(int displayId) throws android.os.RemoteException
    {
      return null;
    }
    /**
     * Move the display's top task onto the user's own screen (display 0), so
     * they can act on it directly when the agent cannot.
     */
    @Override public boolean moveToUser(int displayId) throws android.os.RemoteException
    {
      return false;
    }
    /** Move it back to this display after the user is done. */
    @Override public boolean moveToAgent(int displayId) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * What is on the user's own screen now: `"<taskId>|<package>"`, or `""`.
     * 
     * An action that starts an app from a background service lands it on the
     * user's display, not this one, and nothing on this display changes. This
     * is how that becomes visible.
     */
    @Override public java.lang.String mainScreenTop() throws android.os.RemoteException
    {
      return null;
    }
    /** Move an arbitrary task here (used to pull a spilled app back). */
    @Override public boolean moveTaskToMe(int displayId, int taskId) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * One gesture of one or two fingers, as flat parallel arrays.
     * 
     * `x`/`y`/`path` describe every point of every finger: `path[i]` is the
     * finger a point belongs to, and the points of one finger are in time
     * order. The fingers start together and run for `durationMs`. Coordinates
     * are the display's own pixels.
     * 
     * `injectTap`/`injectSwipe` go through `input -d`, which only ever sends a
     * single straight stroke. A pinch is two fingers moving apart at the same
     * time, and that has no shell equivalent at all.
     */
    @Override public boolean injectGesture(int displayId, int[] x, int[] y, int[] path, int durationMs) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * Put `text` on the device clipboard and paste it with Ctrl+V on the
     * display, then put the user's own clipboard back.
     * 
     * `input -d text` types through the key character map and mangles anything
     * outside ASCII; a clipboard paste carries any character the app can
     * render.
     * 
     * `replace` false means append: the field's existing text is kept and the
     * paste lands after it. This is the form a caller should use on the user's
     * own screen, where the clipboard belongs to them — a replace opens with
     * Ctrl+A, and would also wipe whatever they had already typed.
     * 
     * Returns 0 when it pasted, 1 when the user's clipboard could not be read,
     * 2 on failure. A 1 means nothing was pressed at all: the text never
     * reached the clipboard, so a paste would have inserted the user's own
     * content into the field.
     */
    @Override public int pasteText(int displayId, java.lang.String text, boolean replace) throws android.os.RemoteException
    {
      return 0;
    }
    /**
     * Start watching the user's screen for `windowMs`, and return the task on
     * top of it right now (or -1).
     * 
     * An action can start an app that lands on the user's screen; a single
     * look afterwards cannot tell that apart from an app the user opened
     * themselves. Watching records what actually rose to the top, and when.
     */
    @Override public int watchMainScreen(int windowMs) throws android.os.RemoteException
    {
      return 0;
    }
    /**
     * What rose to the top of the user's screen since `sinceUptimeMs`
     * (`SystemClock.uptimeMillis`), as JSON: an array of
     * `{task, package, at, from, sameTask, standard, now}`, oldest first.
     * 
     * `from` is the display the task was last seen on (-1 when new), `now` is
     * where it is now (-1 when it is gone), `sameTask` marks the same task
     * showing a different page.
     */
    @Override public java.lang.String mainArrivals(long sinceUptimeMs) throws android.os.RemoteException
    {
      return null;
    }
    /**
     * The display's latest frame as a coarse brightness grid: `columns`,
     * `rows`, then one byte per cell, row-major.
     * 
     * A cheap "did the picture change" that works on screens the accessibility
     * tree describes nothing on — a game, a map, a camera preview.
     */
    @Override public byte[] frameSignature(int displayId) throws android.os.RemoteException
    {
      return null;
    }
    /**
     * The most displays this device will run at once, and why not more.
     * 
     * Parallel tasks need per-display focus (API 34+); a low-memory phone gets
     * one either way, and once a second display has failed to appear in this
     * session nothing retries it.
     */
    @Override public java.lang.String parallelLimit() throws android.os.RemoteException
    {
      return null;
    }
    /** How many displays this helper currently owns. */
    @Override public int displayCount() throws android.os.RemoteException
    {
      return 0;
    }
    /**
     * Silence, or un-silence, what `displayId` shows.
     * 
     * The app-op is `PLAY_AUDIO`, per package: an app running on the agent's
     * display is one the user cannot see, so a chime or a video from it is
     * sound with no visible source. The helper remembers the mode each package
     * was in before silencing it and writes that back — never an assumed
     * default — and only ever touches packages it recorded itself.
     * 
     * Returns whether the request took effect. False is not fatal to the
     * display: the app is already running, it is merely audible.
     */
    @Override public boolean setAudioMuted(int displayId, java.lang.String packageName, boolean muted) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * Mark `displayId` as the person's, or take it back.
     * 
     * While it is theirs, every write to it — tap, swipe, key, text, paste,
     * gesture, launch, task moves — is refused by the helper. Reads keep
     * working: the session that handed it over has to be able to look at what
     * they did. This is the on-device half of the takeover guarantee; the
     * process-wide lock in Rust is the host-side half, and it cannot cover a
     * session that connected while a handoff was already in flight.
     * 
     * `holdMs` bounds the takeover: after that long the helper releases it on
     * its own. The caller is expected to take it back first — this is the
     * backstop for a caller that died or was cancelled mid-handoff, which
     * would otherwise leave the display unwritable for the rest of the session.
     * 
     * Returns false from `giveToUser` when there is no such display, and true
     * when it is theirs (including when it already was).
     */
    @Override public boolean giveToUser(int displayId, long holdMs) throws android.os.RemoteException
    {
      return false;
    }
    /** The person is done; the agent may write to the display again. */
    @Override public void takeBack(int displayId) throws android.os.RemoteException
    {
    }
    /**
     * Whether a person holds `displayId` right now.
     * 
     * A refused write answers `false` either way — that is what every write
     * already returns for a display it cannot reach. This is what lets the
     * caller say *why*: "somebody is using it" is a wait, and "it did not work"
     * is a retry, and they are different instructions to give a model.
     */
    @Override public boolean isHeldByUser(int displayId) throws android.os.RemoteException
    {
      return false;
    }
    /**
     * Whether the phone is in a state where the agent's writes still mean what
     * it thinks they mean.
     * 
     * Three things stop that being true, and only the first is visible from
     * the host: this helper being reachable at all (a call that answers proves
     * it), the user's screen being locked (a tap on the user's screen lands on
     * the lock screen, and an app the agent opened may be behind it), and a
     * display no longer holding the app it was holding (the user pressed home
     * on it, or the task moved elsewhere).
     * 
     * JSON:
     * `{locked, main:{task,package}, displays:[{id,task,package}]}`.
     * 
     * Refusing to act is the caller's decision — this reports the state and
     * nothing else, so the two halves can be tested apart.
     */
    @Override public java.lang.String readiness() throws android.os.RemoteException
    {
      return null;
    }
    @Override
    public android.os.IBinder asBinder() {
      return null;
    }
  }
  /** Local-side IPC implementation stub class. */
  public static abstract class Stub extends android.os.Binder implements net.syscity.desktop.shell.ISyscityShell
  {
    /** Construct the stub at attach it to the interface. */
    @SuppressWarnings("this-escape")
    public Stub()
    {
      this.attachInterface(this, DESCRIPTOR);
    }
    /**
     * Cast an IBinder object into an net.syscity.desktop.shell.ISyscityShell interface,
     * generating a proxy if needed.
     */
    public static net.syscity.desktop.shell.ISyscityShell asInterface(android.os.IBinder obj)
    {
      if ((obj==null)) {
        return null;
      }
      android.os.IInterface iin = obj.queryLocalInterface(DESCRIPTOR);
      if (((iin!=null)&&(iin instanceof net.syscity.desktop.shell.ISyscityShell))) {
        return ((net.syscity.desktop.shell.ISyscityShell)iin);
      }
      return new net.syscity.desktop.shell.ISyscityShell.Stub.Proxy(obj);
    }
    @Override public android.os.IBinder asBinder()
    {
      return this;
    }
    @Override public boolean onTransact(int code, android.os.Parcel data, android.os.Parcel reply, int flags) throws android.os.RemoteException
    {
      java.lang.String descriptor = DESCRIPTOR;
      if (code >= android.os.IBinder.FIRST_CALL_TRANSACTION && code <= android.os.IBinder.LAST_CALL_TRANSACTION) {
        data.enforceInterface(descriptor);
      }
      if (code == INTERFACE_TRANSACTION) {
        reply.writeString(descriptor);
        return true;
      }
      switch (code)
      {
        case TRANSACTION_identity:
        {
          int[] _result = this.identity();
          reply.writeNoException();
          reply.writeIntArray(_result);
          break;
        }
        case TRANSACTION_createDisplay:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _arg1;
          _arg1 = data.readInt();
          int _arg2;
          _arg2 = data.readInt();
          int _result = this.createDisplay(_arg0, _arg1, _arg2);
          reply.writeNoException();
          reply.writeInt(_result);
          break;
        }
        case TRANSACTION_startApp:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _arg1;
          _arg1 = data.readString();
          int _result = this.startApp(_arg0, _arg1);
          reply.writeNoException();
          reply.writeInt(_result);
          break;
        }
        case TRANSACTION_releaseDisplay:
        {
          int _arg0;
          _arg0 = data.readInt();
          this.releaseDisplay(_arg0);
          reply.writeNoException();
          break;
        }
        case TRANSACTION_injectTap:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _arg1;
          _arg1 = data.readInt();
          int _arg2;
          _arg2 = data.readInt();
          boolean _result = this.injectTap(_arg0, _arg1, _arg2);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_injectSwipe:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _arg1;
          _arg1 = data.readInt();
          int _arg2;
          _arg2 = data.readInt();
          int _arg3;
          _arg3 = data.readInt();
          int _arg4;
          _arg4 = data.readInt();
          int _arg5;
          _arg5 = data.readInt();
          boolean _result = this.injectSwipe(_arg0, _arg1, _arg2, _arg3, _arg4, _arg5);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_injectKey:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _arg1;
          _arg1 = data.readInt();
          boolean _result = this.injectKey(_arg0, _arg1);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_injectText:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _arg1;
          _arg1 = data.readString();
          boolean _result = this.injectText(_arg0, _arg1);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_frame:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _result = this.frame(_arg0);
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        case TRANSACTION_describe:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _result = this.describe(_arg0);
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        case TRANSACTION_moveToUser:
        {
          int _arg0;
          _arg0 = data.readInt();
          boolean _result = this.moveToUser(_arg0);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_moveToAgent:
        {
          int _arg0;
          _arg0 = data.readInt();
          boolean _result = this.moveToAgent(_arg0);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_mainScreenTop:
        {
          java.lang.String _result = this.mainScreenTop();
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        case TRANSACTION_moveTaskToMe:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _arg1;
          _arg1 = data.readInt();
          boolean _result = this.moveTaskToMe(_arg0, _arg1);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_injectGesture:
        {
          int _arg0;
          _arg0 = data.readInt();
          int[] _arg1;
          _arg1 = data.createIntArray();
          int[] _arg2;
          _arg2 = data.createIntArray();
          int[] _arg3;
          _arg3 = data.createIntArray();
          int _arg4;
          _arg4 = data.readInt();
          boolean _result = this.injectGesture(_arg0, _arg1, _arg2, _arg3, _arg4);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_pasteText:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _arg1;
          _arg1 = data.readString();
          boolean _arg2;
          _arg2 = (0!=data.readInt());
          int _result = this.pasteText(_arg0, _arg1, _arg2);
          reply.writeNoException();
          reply.writeInt(_result);
          break;
        }
        case TRANSACTION_watchMainScreen:
        {
          int _arg0;
          _arg0 = data.readInt();
          int _result = this.watchMainScreen(_arg0);
          reply.writeNoException();
          reply.writeInt(_result);
          break;
        }
        case TRANSACTION_mainArrivals:
        {
          long _arg0;
          _arg0 = data.readLong();
          java.lang.String _result = this.mainArrivals(_arg0);
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        case TRANSACTION_frameSignature:
        {
          int _arg0;
          _arg0 = data.readInt();
          byte[] _result = this.frameSignature(_arg0);
          reply.writeNoException();
          reply.writeByteArray(_result);
          break;
        }
        case TRANSACTION_parallelLimit:
        {
          java.lang.String _result = this.parallelLimit();
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        case TRANSACTION_displayCount:
        {
          int _result = this.displayCount();
          reply.writeNoException();
          reply.writeInt(_result);
          break;
        }
        case TRANSACTION_setAudioMuted:
        {
          int _arg0;
          _arg0 = data.readInt();
          java.lang.String _arg1;
          _arg1 = data.readString();
          boolean _arg2;
          _arg2 = (0!=data.readInt());
          boolean _result = this.setAudioMuted(_arg0, _arg1, _arg2);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_giveToUser:
        {
          int _arg0;
          _arg0 = data.readInt();
          long _arg1;
          _arg1 = data.readLong();
          boolean _result = this.giveToUser(_arg0, _arg1);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_takeBack:
        {
          int _arg0;
          _arg0 = data.readInt();
          this.takeBack(_arg0);
          reply.writeNoException();
          break;
        }
        case TRANSACTION_isHeldByUser:
        {
          int _arg0;
          _arg0 = data.readInt();
          boolean _result = this.isHeldByUser(_arg0);
          reply.writeNoException();
          reply.writeInt(((_result)?(1):(0)));
          break;
        }
        case TRANSACTION_readiness:
        {
          java.lang.String _result = this.readiness();
          reply.writeNoException();
          reply.writeString(_result);
          break;
        }
        default:
        {
          return super.onTransact(code, data, reply, flags);
        }
      }
      return true;
    }
    private static class Proxy implements net.syscity.desktop.shell.ISyscityShell
    {
      private android.os.IBinder mRemote;
      Proxy(android.os.IBinder remote)
      {
        mRemote = remote;
      }
      @Override public android.os.IBinder asBinder()
      {
        return mRemote;
      }
      public java.lang.String getInterfaceDescriptor()
      {
        return DESCRIPTOR;
      }
      /** `[uid, pid]` of the process this Binder lives in; checked before it is accepted. */
      @Override public int[] identity() throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int[] _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          boolean _status = mRemote.transact(Stub.TRANSACTION_identity, _data, _reply, 0);
          _reply.readException();
          _result = _reply.createIntArray();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Create a display and return its id, or -1. */
      @Override public int createDisplay(int width, int height, int dpi) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(width);
          _data.writeInt(height);
          _data.writeInt(dpi);
          boolean _status = mRemote.transact(Stub.TRANSACTION_createDisplay, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readInt();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Put `package` on the display. Returns 0 on success. */
      @Override public int startApp(int displayId, java.lang.String packageName) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeString(packageName);
          boolean _status = mRemote.transact(Stub.TRANSACTION_startApp, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readInt();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Destroy a display and everything on it. */
      @Override public void releaseDisplay(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_releaseDisplay, _data, _reply, 0);
          _reply.readException();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
      }
      /** Tap at a point on the display. */
      @Override public boolean injectTap(int displayId, int x, int y) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeInt(x);
          _data.writeInt(y);
          boolean _status = mRemote.transact(Stub.TRANSACTION_injectTap, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Swipe on the display. */
      @Override public boolean injectSwipe(int displayId, int x1, int y1, int x2, int y2, int durationMs) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeInt(x1);
          _data.writeInt(y1);
          _data.writeInt(x2);
          _data.writeInt(y2);
          _data.writeInt(durationMs);
          boolean _status = mRemote.transact(Stub.TRANSACTION_injectSwipe, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Press a key on the display. */
      @Override public boolean injectKey(int displayId, int keyCode) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeInt(keyCode);
          boolean _status = mRemote.transact(Stub.TRANSACTION_injectKey, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Type text into whatever has focus on the display. */
      @Override public boolean injectText(int displayId, java.lang.String text) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeString(text);
          boolean _status = mRemote.transact(Stub.TRANSACTION_injectText, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * The display's current frame, written to a PNG file.
       * 
       * Returns the file's path — the reader is the app's own process and opens it
       * directly — or null when the display produced nothing.
       */
      @Override public java.lang.String frame(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_frame, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** The app's own debug surface: which overload created this display, for logs. */
      @Override public java.lang.String describe(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_describe, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Move the display's top task onto the user's own screen (display 0), so
       * they can act on it directly when the agent cannot.
       */
      @Override public boolean moveToUser(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_moveToUser, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Move it back to this display after the user is done. */
      @Override public boolean moveToAgent(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_moveToAgent, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * What is on the user's own screen now: `"<taskId>|<package>"`, or `""`.
       * 
       * An action that starts an app from a background service lands it on the
       * user's display, not this one, and nothing on this display changes. This
       * is how that becomes visible.
       */
      @Override public java.lang.String mainScreenTop() throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          boolean _status = mRemote.transact(Stub.TRANSACTION_mainScreenTop, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** Move an arbitrary task here (used to pull a spilled app back). */
      @Override public boolean moveTaskToMe(int displayId, int taskId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeInt(taskId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_moveTaskToMe, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * One gesture of one or two fingers, as flat parallel arrays.
       * 
       * `x`/`y`/`path` describe every point of every finger: `path[i]` is the
       * finger a point belongs to, and the points of one finger are in time
       * order. The fingers start together and run for `durationMs`. Coordinates
       * are the display's own pixels.
       * 
       * `injectTap`/`injectSwipe` go through `input -d`, which only ever sends a
       * single straight stroke. A pinch is two fingers moving apart at the same
       * time, and that has no shell equivalent at all.
       */
      @Override public boolean injectGesture(int displayId, int[] x, int[] y, int[] path, int durationMs) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeIntArray(x);
          _data.writeIntArray(y);
          _data.writeIntArray(path);
          _data.writeInt(durationMs);
          boolean _status = mRemote.transact(Stub.TRANSACTION_injectGesture, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Put `text` on the device clipboard and paste it with Ctrl+V on the
       * display, then put the user's own clipboard back.
       * 
       * `input -d text` types through the key character map and mangles anything
       * outside ASCII; a clipboard paste carries any character the app can
       * render.
       * 
       * `replace` false means append: the field's existing text is kept and the
       * paste lands after it. This is the form a caller should use on the user's
       * own screen, where the clipboard belongs to them — a replace opens with
       * Ctrl+A, and would also wipe whatever they had already typed.
       * 
       * Returns 0 when it pasted, 1 when the user's clipboard could not be read,
       * 2 on failure. A 1 means nothing was pressed at all: the text never
       * reached the clipboard, so a paste would have inserted the user's own
       * content into the field.
       */
      @Override public int pasteText(int displayId, java.lang.String text, boolean replace) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeString(text);
          _data.writeInt(((replace)?(1):(0)));
          boolean _status = mRemote.transact(Stub.TRANSACTION_pasteText, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readInt();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Start watching the user's screen for `windowMs`, and return the task on
       * top of it right now (or -1).
       * 
       * An action can start an app that lands on the user's screen; a single
       * look afterwards cannot tell that apart from an app the user opened
       * themselves. Watching records what actually rose to the top, and when.
       */
      @Override public int watchMainScreen(int windowMs) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(windowMs);
          boolean _status = mRemote.transact(Stub.TRANSACTION_watchMainScreen, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readInt();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * What rose to the top of the user's screen since `sinceUptimeMs`
       * (`SystemClock.uptimeMillis`), as JSON: an array of
       * `{task, package, at, from, sameTask, standard, now}`, oldest first.
       * 
       * `from` is the display the task was last seen on (-1 when new), `now` is
       * where it is now (-1 when it is gone), `sameTask` marks the same task
       * showing a different page.
       */
      @Override public java.lang.String mainArrivals(long sinceUptimeMs) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeLong(sinceUptimeMs);
          boolean _status = mRemote.transact(Stub.TRANSACTION_mainArrivals, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * The display's latest frame as a coarse brightness grid: `columns`,
       * `rows`, then one byte per cell, row-major.
       * 
       * A cheap "did the picture change" that works on screens the accessibility
       * tree describes nothing on — a game, a map, a camera preview.
       */
      @Override public byte[] frameSignature(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        byte[] _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_frameSignature, _data, _reply, 0);
          _reply.readException();
          _result = _reply.createByteArray();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * The most displays this device will run at once, and why not more.
       * 
       * Parallel tasks need per-display focus (API 34+); a low-memory phone gets
       * one either way, and once a second display has failed to appear in this
       * session nothing retries it.
       */
      @Override public java.lang.String parallelLimit() throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          boolean _status = mRemote.transact(Stub.TRANSACTION_parallelLimit, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** How many displays this helper currently owns. */
      @Override public int displayCount() throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        int _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          boolean _status = mRemote.transact(Stub.TRANSACTION_displayCount, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readInt();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Silence, or un-silence, what `displayId` shows.
       * 
       * The app-op is `PLAY_AUDIO`, per package: an app running on the agent's
       * display is one the user cannot see, so a chime or a video from it is
       * sound with no visible source. The helper remembers the mode each package
       * was in before silencing it and writes that back — never an assumed
       * default — and only ever touches packages it recorded itself.
       * 
       * Returns whether the request took effect. False is not fatal to the
       * display: the app is already running, it is merely audible.
       */
      @Override public boolean setAudioMuted(int displayId, java.lang.String packageName, boolean muted) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeString(packageName);
          _data.writeInt(((muted)?(1):(0)));
          boolean _status = mRemote.transact(Stub.TRANSACTION_setAudioMuted, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Mark `displayId` as the person's, or take it back.
       * 
       * While it is theirs, every write to it — tap, swipe, key, text, paste,
       * gesture, launch, task moves — is refused by the helper. Reads keep
       * working: the session that handed it over has to be able to look at what
       * they did. This is the on-device half of the takeover guarantee; the
       * process-wide lock in Rust is the host-side half, and it cannot cover a
       * session that connected while a handoff was already in flight.
       * 
       * `holdMs` bounds the takeover: after that long the helper releases it on
       * its own. The caller is expected to take it back first — this is the
       * backstop for a caller that died or was cancelled mid-handoff, which
       * would otherwise leave the display unwritable for the rest of the session.
       * 
       * Returns false from `giveToUser` when there is no such display, and true
       * when it is theirs (including when it already was).
       */
      @Override public boolean giveToUser(int displayId, long holdMs) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          _data.writeLong(holdMs);
          boolean _status = mRemote.transact(Stub.TRANSACTION_giveToUser, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /** The person is done; the agent may write to the display again. */
      @Override public void takeBack(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_takeBack, _data, _reply, 0);
          _reply.readException();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
      }
      /**
       * Whether a person holds `displayId` right now.
       * 
       * A refused write answers `false` either way — that is what every write
       * already returns for a display it cannot reach. This is what lets the
       * caller say *why*: "somebody is using it" is a wait, and "it did not work"
       * is a retry, and they are different instructions to give a model.
       */
      @Override public boolean isHeldByUser(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        boolean _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_isHeldByUser, _data, _reply, 0);
          _reply.readException();
          _result = (0!=_reply.readInt());
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
      /**
       * Whether the phone is in a state where the agent's writes still mean what
       * it thinks they mean.
       * 
       * Three things stop that being true, and only the first is visible from
       * the host: this helper being reachable at all (a call that answers proves
       * it), the user's screen being locked (a tap on the user's screen lands on
       * the lock screen, and an app the agent opened may be behind it), and a
       * display no longer holding the app it was holding (the user pressed home
       * on it, or the task moved elsewhere).
       * 
       * JSON:
       * `{locked, main:{task,package}, displays:[{id,task,package}]}`.
       * 
       * Refusing to act is the caller's decision — this reports the state and
       * nothing else, so the two halves can be tested apart.
       */
      @Override public java.lang.String readiness() throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        java.lang.String _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          boolean _status = mRemote.transact(Stub.TRANSACTION_readiness, _data, _reply, 0);
          _reply.readException();
          _result = _reply.readString();
        }
        finally {
          _reply.recycle();
          _data.recycle();
        }
        return _result;
      }
    }
    static final int TRANSACTION_identity = (android.os.IBinder.FIRST_CALL_TRANSACTION + 1);
    static final int TRANSACTION_createDisplay = (android.os.IBinder.FIRST_CALL_TRANSACTION + 2);
    static final int TRANSACTION_startApp = (android.os.IBinder.FIRST_CALL_TRANSACTION + 3);
    static final int TRANSACTION_releaseDisplay = (android.os.IBinder.FIRST_CALL_TRANSACTION + 4);
    static final int TRANSACTION_injectTap = (android.os.IBinder.FIRST_CALL_TRANSACTION + 5);
    static final int TRANSACTION_injectSwipe = (android.os.IBinder.FIRST_CALL_TRANSACTION + 6);
    static final int TRANSACTION_injectKey = (android.os.IBinder.FIRST_CALL_TRANSACTION + 7);
    static final int TRANSACTION_injectText = (android.os.IBinder.FIRST_CALL_TRANSACTION + 8);
    static final int TRANSACTION_frame = (android.os.IBinder.FIRST_CALL_TRANSACTION + 9);
    static final int TRANSACTION_describe = (android.os.IBinder.FIRST_CALL_TRANSACTION + 10);
    static final int TRANSACTION_moveToUser = (android.os.IBinder.FIRST_CALL_TRANSACTION + 11);
    static final int TRANSACTION_moveToAgent = (android.os.IBinder.FIRST_CALL_TRANSACTION + 12);
    static final int TRANSACTION_mainScreenTop = (android.os.IBinder.FIRST_CALL_TRANSACTION + 13);
    static final int TRANSACTION_moveTaskToMe = (android.os.IBinder.FIRST_CALL_TRANSACTION + 14);
    static final int TRANSACTION_injectGesture = (android.os.IBinder.FIRST_CALL_TRANSACTION + 15);
    static final int TRANSACTION_pasteText = (android.os.IBinder.FIRST_CALL_TRANSACTION + 16);
    static final int TRANSACTION_watchMainScreen = (android.os.IBinder.FIRST_CALL_TRANSACTION + 17);
    static final int TRANSACTION_mainArrivals = (android.os.IBinder.FIRST_CALL_TRANSACTION + 18);
    static final int TRANSACTION_frameSignature = (android.os.IBinder.FIRST_CALL_TRANSACTION + 19);
    static final int TRANSACTION_parallelLimit = (android.os.IBinder.FIRST_CALL_TRANSACTION + 20);
    static final int TRANSACTION_displayCount = (android.os.IBinder.FIRST_CALL_TRANSACTION + 21);
    static final int TRANSACTION_setAudioMuted = (android.os.IBinder.FIRST_CALL_TRANSACTION + 22);
    static final int TRANSACTION_giveToUser = (android.os.IBinder.FIRST_CALL_TRANSACTION + 23);
    static final int TRANSACTION_takeBack = (android.os.IBinder.FIRST_CALL_TRANSACTION + 24);
    static final int TRANSACTION_isHeldByUser = (android.os.IBinder.FIRST_CALL_TRANSACTION + 25);
    static final int TRANSACTION_readiness = (android.os.IBinder.FIRST_CALL_TRANSACTION + 26);
  }
  /** @hide */
  public static final java.lang.String DESCRIPTOR = "net.syscity.desktop.shell.ISyscityShell";
  /** `[uid, pid]` of the process this Binder lives in; checked before it is accepted. */
  public int[] identity() throws android.os.RemoteException;
  /** Create a display and return its id, or -1. */
  public int createDisplay(int width, int height, int dpi) throws android.os.RemoteException;
  /** Put `package` on the display. Returns 0 on success. */
  public int startApp(int displayId, java.lang.String packageName) throws android.os.RemoteException;
  /** Destroy a display and everything on it. */
  public void releaseDisplay(int displayId) throws android.os.RemoteException;
  /** Tap at a point on the display. */
  public boolean injectTap(int displayId, int x, int y) throws android.os.RemoteException;
  /** Swipe on the display. */
  public boolean injectSwipe(int displayId, int x1, int y1, int x2, int y2, int durationMs) throws android.os.RemoteException;
  /** Press a key on the display. */
  public boolean injectKey(int displayId, int keyCode) throws android.os.RemoteException;
  /** Type text into whatever has focus on the display. */
  public boolean injectText(int displayId, java.lang.String text) throws android.os.RemoteException;
  /**
   * The display's current frame, written to a PNG file.
   * 
   * Returns the file's path — the reader is the app's own process and opens it
   * directly — or null when the display produced nothing.
   */
  public java.lang.String frame(int displayId) throws android.os.RemoteException;
  /** The app's own debug surface: which overload created this display, for logs. */
  public java.lang.String describe(int displayId) throws android.os.RemoteException;
  /**
   * Move the display's top task onto the user's own screen (display 0), so
   * they can act on it directly when the agent cannot.
   */
  public boolean moveToUser(int displayId) throws android.os.RemoteException;
  /** Move it back to this display after the user is done. */
  public boolean moveToAgent(int displayId) throws android.os.RemoteException;
  /**
   * What is on the user's own screen now: `"<taskId>|<package>"`, or `""`.
   * 
   * An action that starts an app from a background service lands it on the
   * user's display, not this one, and nothing on this display changes. This
   * is how that becomes visible.
   */
  public java.lang.String mainScreenTop() throws android.os.RemoteException;
  /** Move an arbitrary task here (used to pull a spilled app back). */
  public boolean moveTaskToMe(int displayId, int taskId) throws android.os.RemoteException;
  /**
   * One gesture of one or two fingers, as flat parallel arrays.
   * 
   * `x`/`y`/`path` describe every point of every finger: `path[i]` is the
   * finger a point belongs to, and the points of one finger are in time
   * order. The fingers start together and run for `durationMs`. Coordinates
   * are the display's own pixels.
   * 
   * `injectTap`/`injectSwipe` go through `input -d`, which only ever sends a
   * single straight stroke. A pinch is two fingers moving apart at the same
   * time, and that has no shell equivalent at all.
   */
  public boolean injectGesture(int displayId, int[] x, int[] y, int[] path, int durationMs) throws android.os.RemoteException;
  /**
   * Put `text` on the device clipboard and paste it with Ctrl+V on the
   * display, then put the user's own clipboard back.
   * 
   * `input -d text` types through the key character map and mangles anything
   * outside ASCII; a clipboard paste carries any character the app can
   * render.
   * 
   * `replace` false means append: the field's existing text is kept and the
   * paste lands after it. This is the form a caller should use on the user's
   * own screen, where the clipboard belongs to them — a replace opens with
   * Ctrl+A, and would also wipe whatever they had already typed.
   * 
   * Returns 0 when it pasted, 1 when the user's clipboard could not be read,
   * 2 on failure. A 1 means nothing was pressed at all: the text never
   * reached the clipboard, so a paste would have inserted the user's own
   * content into the field.
   */
  public int pasteText(int displayId, java.lang.String text, boolean replace) throws android.os.RemoteException;
  /**
   * Start watching the user's screen for `windowMs`, and return the task on
   * top of it right now (or -1).
   * 
   * An action can start an app that lands on the user's screen; a single
   * look afterwards cannot tell that apart from an app the user opened
   * themselves. Watching records what actually rose to the top, and when.
   */
  public int watchMainScreen(int windowMs) throws android.os.RemoteException;
  /**
   * What rose to the top of the user's screen since `sinceUptimeMs`
   * (`SystemClock.uptimeMillis`), as JSON: an array of
   * `{task, package, at, from, sameTask, standard, now}`, oldest first.
   * 
   * `from` is the display the task was last seen on (-1 when new), `now` is
   * where it is now (-1 when it is gone), `sameTask` marks the same task
   * showing a different page.
   */
  public java.lang.String mainArrivals(long sinceUptimeMs) throws android.os.RemoteException;
  /**
   * The display's latest frame as a coarse brightness grid: `columns`,
   * `rows`, then one byte per cell, row-major.
   * 
   * A cheap "did the picture change" that works on screens the accessibility
   * tree describes nothing on — a game, a map, a camera preview.
   */
  public byte[] frameSignature(int displayId) throws android.os.RemoteException;
  /**
   * The most displays this device will run at once, and why not more.
   * 
   * Parallel tasks need per-display focus (API 34+); a low-memory phone gets
   * one either way, and once a second display has failed to appear in this
   * session nothing retries it.
   */
  public java.lang.String parallelLimit() throws android.os.RemoteException;
  /** How many displays this helper currently owns. */
  public int displayCount() throws android.os.RemoteException;
  /**
   * Silence, or un-silence, what `displayId` shows.
   * 
   * The app-op is `PLAY_AUDIO`, per package: an app running on the agent's
   * display is one the user cannot see, so a chime or a video from it is
   * sound with no visible source. The helper remembers the mode each package
   * was in before silencing it and writes that back — never an assumed
   * default — and only ever touches packages it recorded itself.
   * 
   * Returns whether the request took effect. False is not fatal to the
   * display: the app is already running, it is merely audible.
   */
  public boolean setAudioMuted(int displayId, java.lang.String packageName, boolean muted) throws android.os.RemoteException;
  /**
   * Mark `displayId` as the person's, or take it back.
   * 
   * While it is theirs, every write to it — tap, swipe, key, text, paste,
   * gesture, launch, task moves — is refused by the helper. Reads keep
   * working: the session that handed it over has to be able to look at what
   * they did. This is the on-device half of the takeover guarantee; the
   * process-wide lock in Rust is the host-side half, and it cannot cover a
   * session that connected while a handoff was already in flight.
   * 
   * `holdMs` bounds the takeover: after that long the helper releases it on
   * its own. The caller is expected to take it back first — this is the
   * backstop for a caller that died or was cancelled mid-handoff, which
   * would otherwise leave the display unwritable for the rest of the session.
   * 
   * Returns false from `giveToUser` when there is no such display, and true
   * when it is theirs (including when it already was).
   */
  public boolean giveToUser(int displayId, long holdMs) throws android.os.RemoteException;
  /** The person is done; the agent may write to the display again. */
  public void takeBack(int displayId) throws android.os.RemoteException;
  /**
   * Whether a person holds `displayId` right now.
   * 
   * A refused write answers `false` either way — that is what every write
   * already returns for a display it cannot reach. This is what lets the
   * caller say *why*: "somebody is using it" is a wait, and "it did not work"
   * is a retry, and they are different instructions to give a model.
   */
  public boolean isHeldByUser(int displayId) throws android.os.RemoteException;
  /**
   * Whether the phone is in a state where the agent's writes still mean what
   * it thinks they mean.
   * 
   * Three things stop that being true, and only the first is visible from
   * the host: this helper being reachable at all (a call that answers proves
   * it), the user's screen being locked (a tap on the user's screen lands on
   * the lock screen, and an app the agent opened may be behind it), and a
   * display no longer holding the app it was holding (the user pressed home
   * on it, or the task moved elsewhere).
   * 
   * JSON:
   * `{locked, main:{task,package}, displays:[{id,task,package}]}`.
   * 
   * Refusing to act is the caller's decision — this reports the state and
   * nothing else, so the two halves can be tested apart.
   */
  public java.lang.String readiness() throws android.os.RemoteException;
}
