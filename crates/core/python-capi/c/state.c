#include "Python.h"
#include "structmember.h"
#include <stdlib.h>

static PyThreadState thread_state = {.gilstate_counter = 1};

static PyThreadState *current(void) {
    thread_state.interp = PyInterpreterState_Get();
    return &thread_state;
}

PyThreadState *PyThreadState_Get(void) {
    return current();
}

PyThreadState *PyThreadState_GetUnchecked(void) {
    return current();
}

PyThreadState *PyGILState_GetThisThreadState(void) {
    return current();
}

PyThreadState *PyThreadState_New(PyInterpreterState *interpreter) {
    (void)interpreter;
    return current();
}

void PyThreadState_Clear(PyThreadState *state) {
    (void)state;
}

void PyThreadState_DeleteCurrent(void) {
}

void PyEval_AcquireThread(PyThreadState *state) {
    (void)state;
}

void PyEval_ReleaseThread(PyThreadState *state) {
    (void)state;
}

PyFrameObject *PyThreadState_GetFrame(PyThreadState *state) {
    (void)state;
    return NULL;
}

PyFrameObject *PyFrame_GetBack(PyFrameObject *frame) {
    (void)frame;
    return NULL;
}

PyCodeObject *PyFrame_GetCode(PyFrameObject *frame) {
    (void)frame;
    PyErr_SetString(PyExc_SystemError, "frame objects are not exposed to the C API");
    return NULL;
}

int PyFrame_GetLineNumber(PyFrameObject *frame) {
    (void)frame;
    return -1;
}

int PyThread_tss_create(Py_tss_t *key) {
    if (key->_is_initialized) {
        return 0;
    }
    void **slot = calloc(1, sizeof(void *));
    if (slot == NULL) {
        return -1;
    }
    key->_key = (unsigned long)(uintptr_t)slot;
    key->_is_initialized = 1;
    return 0;
}

void PyThread_tss_delete(Py_tss_t *key) {
    if (!key->_is_initialized) {
        return;
    }
    free((void *)(uintptr_t)key->_key);
    key->_key = 0;
    key->_is_initialized = 0;
}

void *PyThread_tss_get(Py_tss_t *key) {
    return *(void **)(uintptr_t)key->_key;
}

int PyThread_tss_set(Py_tss_t *key, void *value) {
    *(void **)(uintptr_t)key->_key = value;
    return 0;
}

Py_tss_t *PyThread_tss_alloc(void) {
    return calloc(1, sizeof(Py_tss_t));
}

void PyThread_tss_free(Py_tss_t *key) {
    PyThread_tss_delete(key);
    free(key);
}

int PyThread_tss_is_created(Py_tss_t *key) {
    return key->_is_initialized;
}

PyObject *PyInterpreterState_GetDict(PyInterpreterState *interpreter) {
    (void)interpreter;
    static PyObject *dict;
    if (dict == NULL) {
        dict = PyDict_New();
    }
    return dict;
}

const char *Py_GetVersion(void) {
    return PY_VERSION " (nanocode)";
}

int PyDict_GetItemStringRef(PyObject *dict, const char *key, PyObject **result) {
    PyObject *name = PyUnicode_FromString(key);
    if (name == NULL) {
        *result = NULL;
        return -1;
    }
    int found = PyDict_GetItemRef(dict, name, result);
    Py_DECREF(name);
    return found;
}

PyObject **_PyObject_GetDictPtr(PyObject *object) {
    Py_ssize_t offset = Py_TYPE(object)->tp_dictoffset;
    if (offset <= 0) {
        return NULL;
    }
    return (PyObject **)((char *)object + offset);
}

int PyObject_VisitManagedDict(PyObject *object, visitproc visit, void *argument) {
    PyObject **dict = _PyObject_GetDictPtr(object);
    if (dict != NULL && *dict != NULL) {
        return visit(*dict, argument);
    }
    return 0;
}

void PyObject_ClearManagedDict(PyObject *object) {
    PyObject **dict = _PyObject_GetDictPtr(object);
    if (dict != NULL) {
        Py_CLEAR(*dict);
    }
}

static void instancemethod_dealloc(PyObject *self) {
    Py_XDECREF(((PyInstanceMethodObject *)self)->func);
    Py_TYPE(self)->tp_free(self);
}

static PyObject *instancemethod_call(PyObject *self, PyObject *args, PyObject *kwargs) {
    return PyObject_Call(((PyInstanceMethodObject *)self)->func, args, kwargs);
}

static PyObject *instancemethod_get(PyObject *self, PyObject *object, PyObject *type) {
    (void)type;
    PyObject *func = ((PyInstanceMethodObject *)self)->func;
    if (object == NULL || object == Py_None) {
        return Py_NewRef(func);
    }
    return PyMethod_New(func, object);
}

static PyObject *instancemethod_doc(PyObject *self, void *closure) {
    (void)closure;
    return PyObject_GetAttrString(((PyInstanceMethodObject *)self)->func, "__doc__");
}

static PyObject *instancemethod_name(PyObject *self, void *closure) {
    (void)closure;
    return PyObject_GetAttrString(((PyInstanceMethodObject *)self)->func, "__name__");
}

static PyMemberDef instancemethod_members[] = {
    {"__func__", Py_T_OBJECT_EX, offsetof(PyInstanceMethodObject, func), Py_READONLY, NULL},
    {NULL, 0, 0, 0, NULL},
};

static PyGetSetDef instancemethod_getset[] = {
    {"__doc__", instancemethod_doc, NULL, NULL, NULL},
    {"__name__", instancemethod_name, NULL, NULL, NULL},
    {NULL, NULL, NULL, NULL, NULL},
};

PyTypeObject PyInstanceMethod_Type = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "instancemethod",
    .tp_basicsize = sizeof(PyInstanceMethodObject),
    .tp_dealloc = instancemethod_dealloc,
    .tp_call = instancemethod_call,
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_members = instancemethod_members,
    .tp_getset = instancemethod_getset,
    .tp_descr_get = instancemethod_get,
};

PyObject *PyInstanceMethod_New(PyObject *func) {
    PyInstanceMethodObject *method = PyObject_New(PyInstanceMethodObject, &PyInstanceMethod_Type);
    if (method == NULL) {
        return NULL;
    }
    method->func = Py_NewRef(func);
    return (PyObject *)method;
}

PyObject *PyInstanceMethod_Function(PyObject *method) {
    return ((PyInstanceMethodObject *)method)->func;
}

PyInterpreterState *PyInterpreterState_Main(void) {
    return PyInterpreterState_Get();
}
