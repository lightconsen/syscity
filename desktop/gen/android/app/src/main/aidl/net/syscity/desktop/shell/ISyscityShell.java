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
     * The display's current frame as a PNG in a file.
     * 
     * Returns a read-only fd the caller must close, or null when the display
     * produced nothing.
     */
    @Override public android.os.ParcelFileDescriptor frame(int displayId) throws android.os.RemoteException
    {
      return null;
    }
    /** The app's own debug surface: which overload created this display, for logs. */
    @Override public java.lang.String describe(int displayId) throws android.os.RemoteException
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
          android.os.ParcelFileDescriptor _result = this.frame(_arg0);
          reply.writeNoException();
          _Parcel.writeTypedObject(reply, _result, android.os.Parcelable.PARCELABLE_WRITE_RETURN_VALUE);
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
       * The display's current frame as a PNG in a file.
       * 
       * Returns a read-only fd the caller must close, or null when the display
       * produced nothing.
       */
      @Override public android.os.ParcelFileDescriptor frame(int displayId) throws android.os.RemoteException
      {
        android.os.Parcel _data = android.os.Parcel.obtain();
        android.os.Parcel _reply = android.os.Parcel.obtain();
        android.os.ParcelFileDescriptor _result;
        try {
          _data.writeInterfaceToken(DESCRIPTOR);
          _data.writeInt(displayId);
          boolean _status = mRemote.transact(Stub.TRANSACTION_frame, _data, _reply, 0);
          _reply.readException();
          _result = _Parcel.readTypedObject(_reply, android.os.ParcelFileDescriptor.CREATOR);
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
   * The display's current frame as a PNG in a file.
   * 
   * Returns a read-only fd the caller must close, or null when the display
   * produced nothing.
   */
  public android.os.ParcelFileDescriptor frame(int displayId) throws android.os.RemoteException;
  /** The app's own debug surface: which overload created this display, for logs. */
  public java.lang.String describe(int displayId) throws android.os.RemoteException;
  /** @hide */
  static class _Parcel {
    static private <T> T readTypedObject(
        android.os.Parcel parcel,
        android.os.Parcelable.Creator<T> c) {
      if (parcel.readInt() != 0) {
          return c.createFromParcel(parcel);
      } else {
          return null;
      }
    }
    static private <T extends android.os.Parcelable> void writeTypedObject(
        android.os.Parcel parcel, T value, int parcelableFlags) {
      if (value != null) {
        parcel.writeInt(1);
        value.writeToParcel(parcel, parcelableFlags);
      } else {
        parcel.writeInt(0);
      }
    }
  }
}
